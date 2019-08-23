use super::{
    protocol::{self, Encode, Message, Terminate},
    Postgres, PostgresQueryParameters, PostgresRow,
};
use crate::{connection::RawConnection, error::Error, query::QueryParameters};
use bytes::{BufMut, BytesMut};
use futures_core::{future::BoxFuture, stream::BoxStream};
use std::{
    io,
    net::{IpAddr, Shutdown, SocketAddr},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use url::Url;

mod establish;
mod execute;
mod fetch;
mod fetch_optional;

pub struct PostgresRawConnection {
    stream: TcpStream,

    // Do we think that there is data in the read buffer to be decoded
    stream_readable: bool,

    // Have we reached end-of-file (been disconnected)
    stream_eof: bool,

    // Buffer used when sending outgoing messages
    pub(super) wbuf: Vec<u8>,

    // Buffer used when reading incoming messages
    // TODO: Evaluate if we _really_ want to use BytesMut here
    rbuf: BytesMut,

    // Process ID of the Backend
    process_id: u32,

    // Backend-unique key to use to send a cancel query message to the server
    secret_key: u32,
}

impl PostgresRawConnection {
    async fn establish(url: &str) -> Result<Self, Error> {
        // TODO: Handle errors
        let url = Url::parse(url).unwrap();

        let host = url.host_str().unwrap_or("localhost");
        let port = url.port().unwrap_or(5432);

        // FIXME: handle errors
        let host: IpAddr = host.parse().unwrap();
        let addr: SocketAddr = (host, port).into();

        let stream = TcpStream::connect(&addr).await.map_err(Error::Io)?;

        let mut conn = Self {
            wbuf: Vec::with_capacity(1024),
            rbuf: BytesMut::with_capacity(1024 * 8),
            stream,
            stream_readable: false,
            stream_eof: false,
            process_id: 0,
            secret_key: 0,
        };

        establish::establish(&mut conn, &url).await?;

        Ok(conn)
    }

    async fn finalize(&mut self) -> Result<(), Error> {
        self.write(Terminate);
        self.flush().await?;
        self.stream.shutdown(Shutdown::Both).map_err(Error::Io)?;

        Ok(())
    }

    // Wait and return the next message to be received from Postgres.
    async fn receive(&mut self) -> Result<Option<Message>, Error> {
        loop {
            if self.stream_eof {
                // Reached end-of-file on a previous read call.
                return Ok(None);
            }

            if self.stream_readable {
                loop {
                    match Message::decode(&mut self.rbuf) {
                        Some(Message::ParameterStatus(_body)) => {
                            // TODO: not sure what to do with these yet
                        }

                        Some(Message::Response(_body)) => {
                            // TODO: Transform Errors+ into an error type and return
                            // TODO: Log all others
                        }

                        Some(message) => {
                            return Ok(Some(message));
                        }

                        None => {
                            // Not enough data in the read buffer to parse a message
                            self.stream_readable = true;
                            break;
                        }
                    }
                }
            }

            // Ensure there is at least 32-bytes of space available
            // in the read buffer so we can safely detect end-of-file
            self.rbuf.reserve(32);

            // SAFE: Read data in directly to buffer without zero-initializing the data.
            //       Postgres is a self-describing format and the TCP frames encode
            //       length headers. We will never attempt to decode more than we
            //       received.
            let n = self
                .stream
                .read(unsafe { self.rbuf.bytes_mut() })
                .await
                .map_err(Error::Io)?;

            // SAFE: After we read in N bytes, we can tell the buffer that it actually
            //       has that many bytes MORE for the decode routines to look at
            unsafe { self.rbuf.advance_mut(n) }

            if n == 0 {
                self.stream_eof = true;
            }

            self.stream_readable = true;
        }
    }

    pub(super) fn write(&mut self, message: impl Encode) {
        message.encode(&mut self.wbuf);
    }

    async fn flush(&mut self) -> Result<(), Error> {
        self.stream.write_all(&self.wbuf).await.map_err(Error::Io)?;
        self.wbuf.clear();

        Ok(())
    }
}

impl RawConnection for PostgresRawConnection {
    type Backend = Postgres;

    #[inline]
    fn establish(url: &str) -> BoxFuture<Result<Self, Error>> {
        Box::pin(PostgresRawConnection::establish(url))
    }

    #[inline]
    fn finalize<'c>(&'c mut self) -> BoxFuture<'c, Result<(), Error>> {
        Box::pin(self.finalize())
    }

    fn execute<'c>(
        &'c mut self,
        query: &str,
        params: PostgresQueryParameters,
    ) -> BoxFuture<'c, Result<u64, Error>> {
        finish(self, query, params, 0);

        Box::pin(execute::execute(self))
    }

    fn fetch<'c>(
        &'c mut self,
        query: &str,
        params: PostgresQueryParameters,
    ) -> BoxStream<'c, Result<PostgresRow, Error>> {
        finish(self, query, params, 0);

        Box::pin(fetch::fetch(self))
    }

    fn fetch_optional<'c>(
        &'c mut self,
        query: &str,
        params: PostgresQueryParameters,
    ) -> BoxFuture<'c, Result<Option<PostgresRow>, Error>> {
        finish(self, query, params, 1);

        Box::pin(fetch_optional::fetch_optional(self))
    }
}

fn finish(conn: &mut PostgresRawConnection, query: &str, params: PostgresQueryParameters, limit: i32) {
    conn.write(protocol::Parse {
        portal: "",
        query,
        param_types: &*params.types,
    });

    conn.write(protocol::Bind {
        portal: "",
        statement: "",
        formats: &[1], // [BINARY]
        // TODO: Early error if there is more than i16
        values_len: params.types.len() as i16,
        values: &*params.buf,
        result_formats: &[1], // [BINARY]
    });

    // TODO: Make limit be 1 for fetch_optional
    conn.write(protocol::Execute {
        portal: "",
        limit,
    });

    conn.write(protocol::Sync);
}
