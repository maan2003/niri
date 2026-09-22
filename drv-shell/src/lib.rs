//! drv-shell's two wires. [`ask`] is for the trusted services that need the person's word
//! (drv-cast, drv-agent), over fds the supervisor made. [`notify`] is for apps, over the
//! shell's world-connectable socket, keyed on the peer uid. Both are one postcard message
//! per `SOCK_SEQPACKET` datagram (`drv_policy::seq`).

pub mod ask;
pub mod notify;
