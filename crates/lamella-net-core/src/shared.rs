//! One network stack, several holders.

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt;

use crate::{InterfaceInfo, Interest, NetBackend, NetResult, SocketHandle};

/// What a re-entrant borrow says. The interesting half is not that it happened but that one
/// backend operation reached the backend again, which names the bug precisely.
const REENTERED: &str =
    "a shared network backend was re-entered: one operation on it called another";

/// A [`NetBackend`] several holders can have at once, over one real backend.
///
/// [`SharedNet::new`] takes the backend and produces the first holder; [`Clone`] produces the
/// rest. The stack lives for as long as any holder does, so a firmware keeps one for its own serve
/// loop and lends clones to each evaluation, and the stack survives every one of them.
pub struct SharedNet(Rc<RefCell<dyn NetBackend>>);

impl SharedNet {
    /// Take ownership of `backend`, and hand back the first holder of it.
    pub fn new<N: NetBackend + 'static>(backend: N) -> Self {
        Self(Rc::new(RefCell::new(backend)))
    }

    /// How many holders this backend currently has, so a caller that expects to be the last one can
    /// check rather than assume.
    #[must_use]
    pub fn holders(&self) -> usize {
        Rc::strong_count(&self.0)
    }

    /// Borrow the backend for the length of one operation.
    ///
    /// # Panics
    ///
    /// If the backend is already borrowed, which means one of its own operations reached it again.
    fn with<T>(&self, operation: impl FnOnce(&mut dyn NetBackend) -> T) -> T {
        let mut backend = self.0.try_borrow_mut().expect(REENTERED);
        operation(&mut *backend)
    }
}

impl Clone for SharedNet {
    fn clone(&self) -> Self {
        Self(Rc::clone(&self.0))
    }
}

impl fmt::Debug for SharedNet {
    /// Borrows only if it can. Debug is what a diagnostic prints while something else has already
    /// gone wrong, which is exactly when a borrow is most likely to be held -- so this must never
    /// be the call that panics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.try_borrow() {
            Ok(backend) => write!(formatter, "SharedNet({backend:?})"),
            Err(_) => formatter.write_str("SharedNet(in use)"),
        }
    }
}

impl NetBackend for SharedNet {
    fn resolve(&mut self, host: &str) -> Vec<Vec<u8>> {
        self.with(|backend| backend.resolve(host))
    }

    fn tcp_connect(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle> {
        self.with(|backend| backend.tcp_connect(addr, port))
    }

    fn connect_check(&mut self, socket: SocketHandle) -> NetResult<()> {
        self.with(|backend| backend.connect_check(socket))
    }

    fn tcp_listen(&mut self, addr: &[u8], port: u16, backlog: i32) -> NetResult<SocketHandle> {
        self.with(|backend| backend.tcp_listen(addr, port, backlog))
    }

    fn accept(&mut self, listener: SocketHandle) -> NetResult<SocketHandle> {
        self.with(|backend| backend.accept(listener))
    }

    fn recv(&mut self, socket: SocketHandle, buf: &mut [u8]) -> NetResult<usize> {
        self.with(|backend| backend.recv(socket, buf))
    }

    fn send(&mut self, socket: SocketHandle, buf: &[u8]) -> NetResult<usize> {
        self.with(|backend| backend.send(socket, buf))
    }

    fn udp_bind(&mut self, addr: &[u8], port: u16) -> NetResult<SocketHandle> {
        self.with(|backend| backend.udp_bind(addr, port))
    }

    fn udp_send_to(
        &mut self,
        socket: SocketHandle,
        buf: &[u8],
        addr: &[u8],
        port: u16,
    ) -> NetResult<usize> {
        self.with(|backend| backend.udp_send_to(socket, buf, addr, port))
    }

    fn udp_recv_from(
        &mut self,
        socket: SocketHandle,
        buf: &mut [u8],
        sender_addr: &mut [u8],
    ) -> NetResult<(usize, usize, u16)> {
        self.with(|backend| backend.udp_recv_from(socket, buf, sender_addr))
    }

    fn local_port(&mut self, socket: SocketHandle) -> Option<u16> {
        self.with(|backend| backend.local_port(socket))
    }

    fn close(&mut self, socket: SocketHandle) {
        self.with(|backend| backend.close(socket));
    }

    fn register(&mut self, socket: SocketHandle, interest: Interest) {
        self.with(|backend| backend.register(socket, interest));
    }

    fn deregister(&mut self, socket: SocketHandle) {
        self.with(|backend| backend.deregister(socket));
    }

    fn poll(&mut self, timeout_ms: Option<u64>) -> Vec<SocketHandle> {
        self.with(|backend| backend.poll(timeout_ms))
    }

    fn network_available(&mut self) -> bool {
        self.with(|backend| backend.network_available())
    }

    fn interface_count(&mut self) -> u32 {
        self.with(|backend| backend.interface_count())
    }

    fn interface_info(&mut self, index: u32) -> Option<InterfaceInfo> {
        self.with(|backend| backend.interface_info(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that counts what it was asked and can be made to reach back into its own handle.
    #[derive(Debug, Default)]
    struct Recorder {
        opened: u32,
        closed: Vec<SocketHandle>,
        available: bool,
        reenter: Option<Rc<RefCell<Option<SharedNet>>>>,
    }

    impl NetBackend for Recorder {
        fn resolve(&mut self, _host: &str) -> Vec<Vec<u8>> {
            if let Some(hook) = self.reenter.clone() {
                if let Some(handle) = hook.borrow_mut().as_mut() {
                    let _ = handle.local_port(0);
                }
            }
            Vec::new()
        }
        fn tcp_connect(&mut self, _addr: &[u8], _port: u16) -> NetResult<SocketHandle> {
            self.opened += 1;
            NetResult::Ready(self.opened)
        }
        fn connect_check(&mut self, _socket: SocketHandle) -> NetResult<()> {
            NetResult::Ready(())
        }
        fn tcp_listen(&mut self, _addr: &[u8], _port: u16, _backlog: i32) -> NetResult<SocketHandle> {
            self.opened += 1;
            NetResult::Ready(self.opened)
        }
        fn accept(&mut self, _listener: SocketHandle) -> NetResult<SocketHandle> {
            NetResult::WouldBlock
        }
        fn recv(&mut self, _socket: SocketHandle, _buf: &mut [u8]) -> NetResult<usize> {
            NetResult::WouldBlock
        }
        fn send(&mut self, _socket: SocketHandle, buf: &[u8]) -> NetResult<usize> {
            NetResult::Ready(buf.len())
        }
        fn udp_bind(&mut self, _addr: &[u8], _port: u16) -> NetResult<SocketHandle> {
            NetResult::Error
        }
        fn udp_send_to(
            &mut self,
            _socket: SocketHandle,
            _buf: &[u8],
            _addr: &[u8],
            _port: u16,
        ) -> NetResult<usize> {
            NetResult::Error
        }
        fn udp_recv_from(
            &mut self,
            _socket: SocketHandle,
            _buf: &mut [u8],
            _sender_addr: &mut [u8],
        ) -> NetResult<(usize, usize, u16)> {
            NetResult::Error
        }
        fn local_port(&mut self, _socket: SocketHandle) -> Option<u16> {
            Some(4242)
        }
        fn close(&mut self, socket: SocketHandle) {
            self.closed.push(socket);
        }
        fn register(&mut self, _socket: SocketHandle, _interest: Interest) {}
        fn deregister(&mut self, _socket: SocketHandle) {}
        fn poll(&mut self, _timeout_ms: Option<u64>) -> Vec<SocketHandle> {
            Vec::new()
        }
        fn network_available(&mut self) -> bool {
            self.available
        }
    }

    fn ready(result: NetResult<SocketHandle>) -> Option<SocketHandle> {
        match result {
            NetResult::Ready(handle) => Some(handle),
            _ => None,
        }
    }

    #[test]
    fn two_holders_are_two_views_of_one_stack_rather_than_two_stacks() {
        let mut serve = SharedNet::new(Recorder::default());
        let mut program = serve.clone();

        assert_eq!(ready(serve.tcp_connect(&[10, 0, 0, 1], 80)), Some(1));
        assert_eq!(
            ready(program.tcp_connect(&[10, 0, 0, 2], 80)),
            Some(2),
            "the second holder continues the first one's numbering, which is one socket table"
        );
        assert_eq!(serve.holders(), 2);
    }

    #[test]
    fn a_holder_going_away_does_not_take_the_stack_with_it() {
        let mut serve = SharedNet::new(Recorder::default());
        let mut evaluation = serve.clone();
        assert_eq!(ready(evaluation.tcp_connect(&[10, 0, 0, 1], 80)), Some(1));

        drop(evaluation);

        assert_eq!(serve.holders(), 1);
        assert_eq!(
            ready(serve.tcp_connect(&[10, 0, 0, 2], 80)),
            Some(2),
            "the stack outlived the evaluation and kept its state"
        );
    }

    #[test]
    fn the_defaulting_methods_report_the_stack_rather_than_the_wrapper() {
        let mut shared = SharedNet::new(Recorder { available: true, ..Recorder::default() });
        assert!(
            shared.network_available(),
            "inheriting the default would say NO NETWORK while the stack underneath has one"
        );
    }

    #[test]
    #[should_panic(expected = "re-entered")]
    fn an_operation_that_reaches_the_backend_again_says_so_instead_of_corrupting_it() {
        let hook: Rc<RefCell<Option<SharedNet>>> = Rc::new(RefCell::new(None));
        let mut shared =
            SharedNet::new(Recorder { reenter: Some(Rc::clone(&hook)), ..Recorder::default() });
        *hook.borrow_mut() = Some(shared.clone());

        let _ = shared.resolve("example.invalid");
    }

    #[test]
    fn debug_is_not_the_call_that_panics() {
        let shared = SharedNet::new(Recorder::default());
        let borrowed = shared.0.borrow_mut();

        let printed = alloc::format!("{shared:?}");

        drop(borrowed);
        assert_eq!(
            printed, "SharedNet(in use)",
            "a diagnostic printed while a borrow is held must still print"
        );
    }
}
