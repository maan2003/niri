//! The wires of drv-agent's doors. The ssh agent's door speaks OpenSSH's agent protocol,
//! nothing of ours; the FIDO door speaks [`fido`].

pub mod fido;
