//! The byte transport under the client.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;

/// A bidirectional byte stream to a remote stub. `TcpStream` in production; a scripted server in
/// tests ([`crate::script::ScriptedConnection`]).
pub trait Connection: Read + Write + Send + fmt::Debug {
    /// A second handle that can write to the same stream from another thread. The client uses it
    /// to send the interrupt byte while a resume request blocks.
    fn interrupt_writer(&self) -> io::Result<Box<dyn Write + Send>>;
}

impl Connection for TcpStream {
    fn interrupt_writer(&self) -> io::Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.try_clone()?))
    }
}
