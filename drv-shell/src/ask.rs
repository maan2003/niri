//! A trusted service asks the person something through the shell. Ids are the asker's;
//! each line has its own. Every string here comes from the asker, never from an app: the
//! shell shows what it is told.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

/// One thing to pick from: `key` comes back; `name` and `detail` are shown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub key: String,
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello { version: u32 },
    /// "`app` wants to `what`", `note` under it: `Yes` or `Cancelled`.
    Confirm { id: u64, app: String, uid: u32, what: String, note: String },
    /// A secret typed at the shell, never in the app: `Secret` or `Cancelled`.
    Secret { id: u64, app: String, uid: u32, what: String, prompt: String },
    /// Shown until the asker's `Cancel`; `Cancelled` if the person refuses first.
    Touch { id: u64, app: String, uid: u32, what: String, prompt: String },
    /// One of `choices`: `Picked` or `Cancelled`. Sent again under the same id, it replaces
    /// the list while the dialog is up or waiting.
    Pick { id: u64, app: String, uid: u32, what: String, note: String, choices: Vec<Choice> },
    /// The asker withdrew it: the dialog goes down, no answer.
    Cancel { id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    Yes { id: u64 },
    Secret { id: u64, secret: String },
    Picked { id: u64, key: String },
    /// The person said no.
    Cancelled { id: u64 },
}

#[cfg(feature = "client")]
mod client {
    use std::collections::HashMap;
    use std::os::fd::OwnedFd;
    use std::process;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    use drv_policy::seq;

    use super::{Request, Response, VERSION};

    /// A service's line to the shell: requests under ids of our own, answers back on a thread
    /// to whoever waits for that id. The shell gone ends the process: nothing works without it.
    pub struct Client {
        who: &'static str,
        out: Mutex<OwnedFd>,
        waiting: Mutex<HashMap<u64, mpsc::Sender<Response>>>,
        next: AtomicU64,
    }

    impl Client {
        /// Hello over the wire, then a thread that hands answers to whoever waits. `who` is us in the log.
        pub fn start(sock: OwnedFd, who: &'static str) -> Result<Arc<Self>, String> {
            seq::send(&sock, &Request::Hello { version: VERSION }, &[])
                .map_err(|e| format!("hello to the shell: {e}"))?;
            let (hello, _) =
                seq::recv::<Response>(&sock).map_err(|e| format!("hello from the shell: {e}"))?;
            match hello {
                Response::Hello { version } if version == VERSION => {}
                Response::Hello { version } => {
                    drv_os::say!("{who}: the shell speaks version {version}, we speak {VERSION}");
                }
                _ => return Err("no hello from the shell".to_owned()),
            }
            let reader = sock.try_clone().map_err(|e| format!("dup: {e}"))?;
            let shell = Arc::new(Self {
                who,
                out: Mutex::new(sock),
                waiting: Mutex::default(),
                next: AtomicU64::new(1),
            });
            let dispatcher = shell.clone();
            std::thread::spawn(move || {
                loop {
                    match seq::recv::<Response>(&reader) {
                        Ok((resp, _)) => dispatcher.dispatch(resp),
                        Err(err) => {
                            drv_os::say!("{who}: the shell: {err}");
                            process::exit(1);
                        }
                    }
                }
            });
            Ok(shell)
        }

        fn dispatch(&self, resp: Response) {
            let id = match &resp {
                Response::Secret { id, .. }
                | Response::Cancelled { id }
                | Response::Yes { id }
                | Response::Picked { id, .. } => *id,
                Response::Hello { .. } => return,
            };
            let waiter = self.waiting.lock().unwrap().remove(&id);
            if let Some(tx) = waiter {
                let _ = tx.send(resp);
            }
        }

        fn send(&self, req: &Request) -> Result<(), String> {
            seq::send(&*self.out.lock().unwrap(), req, &[]).map_err(|e| format!("the shell: {e}"))
        }

        /// A request under a fresh id, and where its answer arrives.
        fn ask(
            &self,
            make: impl FnOnce(u64) -> Request,
        ) -> Result<(u64, mpsc::Receiver<Response>), String> {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = mpsc::channel();
            self.waiting.lock().unwrap().insert(id, tx);
            if let Err(err) = self.send(&make(id)) {
                self.waiting.lock().unwrap().remove(&id);
                return Err(err);
            }
            Ok((id, rx))
        }

        /// The PIN the person typed, or None if they refused. `what` follows "`app` wants to".
        pub fn pin(&self, app: &str, uid: u32, what: &str, prompt: &str) -> Result<Option<String>, String> {
            let (app, what, prompt) = (app.to_owned(), what.to_owned(), prompt.to_owned());
            let (_, rx) = self.ask(|id| Request::Secret {
                id,
                app,
                uid,
                what,
                prompt,
            })?;
            match rx.recv() {
                Ok(Response::Secret { secret, .. }) => Ok(Some(secret)),
                Ok(Response::Cancelled { .. }) => Ok(None),
                Ok(_) => Err("the shell answered something else".to_owned()),
                Err(_) => Err("the shell is gone".to_owned()),
            }
        }

        /// A touch prompt, up until what this returns is dropped.
        pub fn touch(
            self: &Arc<Self>,
            app: &str,
            uid: u32,
            what: &str,
            prompt: &str,
        ) -> Result<Touching, String> {
            let (app, what, prompt) = (app.to_owned(), what.to_owned(), prompt.to_owned());
            let (id, rx) = self.ask(|id| Request::Touch {
                id,
                app,
                uid,
                what,
                prompt,
            })?;
            Ok(Touching {
                shell: self.clone(),
                id,
                rx,
            })
        }
    }

    pub struct Touching {
        shell: Arc<Client>,
        id: u64,
        rx: mpsc::Receiver<Response>,
    }

    impl Touching {
        /// Whether the person has refused (Escape at the dialog) since the last look.
        pub fn refused(&self) -> bool {
            matches!(self.rx.try_recv(), Ok(Response::Cancelled { .. }))
        }
    }

    impl Drop for Touching {
        fn drop(&mut self) {
            self.shell.waiting.lock().unwrap().remove(&self.id);
            if let Err(err) = self.shell.send(&Request::Cancel { id: self.id }) {
                drv_os::say!("{}: {err}", self.shell.who);
            }
        }
    }
}
#[cfg(feature = "client")]
pub use client::{Client, Touching};
