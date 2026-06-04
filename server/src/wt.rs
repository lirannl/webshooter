use std::io::{Error as IOError, Write};
use tokio::io::AsyncWrite;
use wtransport::{Connection, VarInt};

pub struct VideoWriter<'a>(&'a Connection);
impl<'a> Write for VideoWriter<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.send_datagram(buf) {
            Ok(_) => Ok(buf.len()),
            Err(err) => Err(IOError::new(std::io::ErrorKind::BrokenPipe, err)),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> AsyncWrite for VideoWriter<'a> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.0.send_datagram(buf) {
            Ok(_) => Ok(buf.len()),
            Err(err) => Err(IOError::new(std::io::ErrorKind::BrokenPipe, err)),
        }
        .into()
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Ok(()).into()
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.0.close(VarInt::from_u32(0), b"shutdown");
        Ok(()).into()
    }
}
impl<'a> From<&'a Connection> for VideoWriter<'a> {
    fn from(conn: &'a Connection) -> Self {
        VideoWriter(conn)
    }
}
