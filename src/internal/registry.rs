use std::collections::HashMap;

use bytes::BytesMut;
use uuid::Uuid;

use internal::command::Cmd;
use internal::connection::Connection;
use internal::messages;
use internal::operations::{ OperationError, OperationWrapper, OperationId, Tracking, Session };
use internal::package::Pkg;

struct Request {
    session: OperationId,
    tracker: Tracking,
}

impl Request {
    fn new(session: OperationId, cmd: Cmd, conn_id: Uuid) -> Request {
        Request {
            session,
            tracker: Tracking::new(cmd, conn_id),
        }
    }

    fn get_id(&self) -> Uuid {
        self.tracker.get_id()
    }
}

struct SessionImpl<'a> {
    id: OperationId,
    assocs: &'a mut HashMap<Uuid, Request>,
    conn: &'a Connection,
    runnings: &'a mut Vec<Uuid>,
}

impl<'a> SessionImpl<'a> {
    fn new(
        id: OperationId,
        assocs: &'a mut HashMap<Uuid, Request>,
        conn: &'a Connection,
        runnings: &'a mut Vec<Uuid>) -> SessionImpl<'a>
    {
        SessionImpl {
            id,
            assocs,
            conn,
            runnings,
        }
    }
}

fn terminate(assocs: &mut HashMap<Uuid, Request>, runnings: Vec<Uuid>) {
    for id in runnings {
        assocs.remove(&id);
    }
}

impl<'a> Session for SessionImpl<'a> {
    fn new_request(&mut self, cmd: Cmd) -> Uuid {
        let req = Request::new(self.id, cmd, self.conn.id);
        let id  = req.get_id();

        self.assocs.insert(id, req);
        self.runnings.push(id);

        id
    }

    fn pop(&mut self, id: &Uuid) -> ::std::io::Result<Tracking> {
        match self.assocs.remove(id) {
            Some(req) => {
                let pos = self.runnings
                              .iter()
                              .position(|x| x == id).unwrap();

                self.runnings.remove(pos);

                Ok(req.tracker)
            },

            None => {
                let error = ::std::io::Error::new(::std::io::ErrorKind::Other, "Tracker must exists");
                Err(error)
            },
        }
    }

    fn reuse(&mut self, tracker: Tracking) {
        let id  = tracker.get_id();
        let req = Request {
            session: self.id,
            tracker,
        };

        self.runnings.push(id);
        self.assocs.insert(id, req);
    }

    fn using(&mut self, id: &Uuid) -> ::std::io::Result<&mut Tracking> {
        match self.assocs.get_mut(id) {
            Some(req) => Ok(&mut req.tracker),
            None      => {
                let error = ::std::io::Error::new(::std::io::ErrorKind::Other, "Tracker must exists");
                Err(error)
            },
        }
    }

    fn requests(&self) -> Vec<&Tracking> {
        self.assocs.values().map(|req| &req.tracker).collect()
    }

    fn terminate(&mut self) {
        terminate(self.assocs, self.runnings.drain(..).collect());
    }

    fn connection_id(&self) -> Uuid {
        self.conn.id
    }

    fn has_running_requests(&self) -> bool {
        !self.runnings.is_empty()
    }
}

struct Requests {
    sessions: HashMap<OperationId, OperationWrapper>,
    session_request_ids: HashMap<OperationId, Vec<Uuid>>,
    assocs: HashMap<Uuid, Request>,
    buffer: BytesMut,
}

impl Requests {
    fn new() -> Requests {
        Requests {
            sessions: HashMap::new(),
            session_request_ids: HashMap::new(),
            assocs: HashMap::new(),
            buffer: BytesMut::new(),
        }
    }

    fn register(&mut self, conn: &Connection, mut op: OperationWrapper) {
        let mut runnings = Vec::new();
        let     success = {
            let session = SessionImpl::new(op.id, &mut self.assocs, conn, &mut runnings);

            match op.send(&mut self.buffer, session).map(|out| out.produced_pkgs()) {
                Ok(pkgs) => {
                    conn.enqueue_all(pkgs);

                    true
                },

                Err(e) => {
                    error!("Exception occured when issuing requests: {}", e);

                    false
                },
            }
        };

        if !success {
            terminate(&mut self.assocs, runnings);
        } else {
            self.session_request_ids.insert(op.id, runnings);
            self.sessions.insert(op.id, op);
        }
    }

    fn handle_pkg(&mut self, conn: &Connection, pkg: Pkg) {
        enum Out {
            Failed,
            Handled,
        }

        let pkg_id  = pkg.correlation;
        let pkg_cmd = pkg.cmd;

        if let Some(req) = self.assocs.remove(&pkg_id) {
            let original_cmd = req.tracker.get_cmd();
            let session_id   = req.session;

            debug!("Package [{}]: command {:?} received {:?}.", pkg_id, original_cmd, pkg_cmd);

            let session_over = {
                let runnings = self.session_request_ids
                                   .get_mut(&req.session)
                                   .expect("No session associated to request!");

                let op = self.sessions
                             .get_mut(&session_id)
                             .expect("Unknown session!");

                // We insert back the request to let the operation decides if it
                // want to keep it.
                self.assocs.insert(pkg_id, req);

                let out = {
                    let mut session =
                        SessionImpl::new(
                            session_id, &mut self.assocs, conn, runnings);

                    match pkg.cmd {
                        Cmd::BadRequest => {
                            let msg = pkg.to_text();

                            error!("Bad request for command {:?}: {}.", original_cmd, msg);

                            op.failed(OperationError::ServerError(Some(msg)));

                            Out::Failed
                        },

                        Cmd::NotAuthenticated => {
                            error!("Not authenticated for command {:?}.", original_cmd);

                            op.failed(OperationError::AuthenticationRequired);

                            Out::Failed
                        },

                        Cmd::NotHandled => {
                            warn!("Not handled request {:?} id {}.", original_cmd, pkg_id);

                            let msg: ::std::io::Result<messages::NotHandled> =
                                pkg.to_message();

                            match msg {
                                Ok(not_handled) => {
                                    match not_handled.get_reason() {
                                        messages::NotHandled_NotHandledReason::NotMaster => {
                                            warn!("Received a non master error on command {:?} id {}.
                                                  This driver doesn't support cluster connection yet.", original_cmd, pkg_id);

                                            op.failed(OperationError::NotImplemented);

                                            Out::Failed
                                        },

                                        _ => {
                                            warn!("The server has either not started or is too busy.
                                                  Retrying command {:?} id {}.", original_cmd, pkg_id);

                                            match op.retry(&mut self.buffer, &mut session, pkg_id) {
                                                Ok(outcome) => {
                                                    let pkgs = outcome.produced_pkgs();

                                                    if !pkgs.is_empty() {
                                                        conn.enqueue_all(pkgs);
                                                    }

                                                    Out::Handled
                                                },

                                                Err(error) => {
                                                    error!(
                                                        "An error occured when retrying command {:?} id {}: {}.",
                                                        original_cmd, pkg_id, error
                                                    );

                                                    Out::Failed
                                                },
                                            }
                                        },
                                    }
                                },

                                Err(error) => {
                                    error!("Decoding error: can't decode NotHandled message: {}.", error);

                                    Out::Failed
                                },
                            }
                        },

                        _ => match op.receive(&mut self.buffer, session, pkg) {
                            Ok(outcome) => {
                                let pkgs = outcome.produced_pkgs();

                                if !pkgs.is_empty() {
                                    conn.enqueue_all(pkgs);
                                }

                                Out::Handled
                            },

                            Err(e) => {
                                error!("An error occured when running operation: {}", e);
                                let msg = format!("Exception raised: {}", e);

                                op.failed(OperationError::InvalidOperation(msg));

                                Out::Failed
                            },
                        },
                    }
                };

                if let Out::Failed = out {
                    terminate(&mut self.assocs, runnings.drain(..).collect());
                }

                runnings.is_empty()
            };

            if session_over {
                self.sessions.remove(&session_id);
                self.session_request_ids.remove(&session_id);
            }
        } else {
            warn!("Package [{}] not handled: cmd {:?}.", pkg_id, pkg_cmd);
        }
    }

    fn check_and_retry(&mut self, conn: &Connection) {
        let mut sessions_to_delete = Vec::new();

        for op in self.sessions.values_mut() {
            let runnings = self
                    .session_request_ids
                    .get_mut(&op.id)
                    .expect("No session associated to requests");

            let result = {
                let session = SessionImpl::new(
                    op.id, &mut self.assocs, conn, runnings);

                op.check_and_retry(&mut self.buffer, session)
            };

            match result {
                Ok(outcome) => {
                    if outcome.is_done() {
                        for id in runnings.drain(..) {
                            self.assocs.remove(&id);
                        }

                        sessions_to_delete.push(op.id);
                    } else {
                        let pkgs = outcome.produced_pkgs();

                        if !pkgs.is_empty() {
                            conn.enqueue_all(pkgs);
                        }
                    }
                },

                Err(e) => {
                    error!("Exception raised when checking out operation: {}", e);
                    let msg = format!("Exception raised: {}", e);

                    op.failed(OperationError::InvalidOperation(msg));

                    for id in runnings.drain(..) {
                        self.assocs.remove(&id);
                    }

                    sessions_to_delete.push(op.id);
                },
            }
        }

        for session_id in sessions_to_delete {
            self.sessions.remove(&session_id);
            self.session_request_ids.remove(&session_id);
        }
    }

    pub(crate) fn abort(&mut self) {
        for op in self.sessions.values_mut() {
            op.failed(OperationError::Aborted);
        }
    }
}

pub(crate) struct Registry {
    requests: Requests,
    awaiting: Vec<OperationWrapper>,
}

impl Registry {
    pub(crate) fn new() -> Registry {
        Registry {
            requests: Requests::new(),
            awaiting: Vec::new(),
        }
    }

    pub(crate) fn register(&mut self, op: OperationWrapper, conn: Option<&Connection>) {
        match conn {
            None       => self.awaiting.push(op),
            Some(conn) => self.requests.register(conn, op),
        }
    }

    pub(crate) fn handle(&mut self, pkg: Pkg, conn: &Connection) {
        self.requests.handle_pkg(conn, pkg);
    }

    pub(crate) fn check_and_retry(&mut self, conn: &Connection) {
        self.requests.check_and_retry(conn);

        while let Some(op) = self.awaiting.pop() {
            self.register(op, Some(conn));
        }
    }

    pub(crate) fn abort(&mut self) {
        self.requests.abort();

        for op in self.awaiting.iter_mut() {
            op.failed(OperationError::Aborted);
        }
    }
}
