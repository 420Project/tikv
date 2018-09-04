// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::{Arc, RwLock};

use grpc::{ChannelBuilder, EnvBuilder, Environment, Server as GrpcServer, ServerBuilder};
use kvproto::debugpb_grpc::create_debug;
use kvproto::import_sstpb_grpc::create_import_sst;
use kvproto::tikvpb_grpc::*;

use coprocessor::{self, EndPointHost, EndPointTask};
use import::ImportSSTService;
use raftstore::store::{Engines, SnapManager};
use storage::{Engine, Storage};
use util::security::SecurityManager;
use util::worker::{Builder as WorkerBuilder, Worker};

use super::raft_client::RaftClient;
use super::readpool::ReadPool;
use super::resolve::StoreAddrResolver;
use super::service::*;
use super::snap::{Runner as SnapHandler, Task as SnapTask};
use super::transport::{RaftStoreRouter, ServerTransport};
use super::{Config, Result};

const DEFAULT_COPROCESSOR_BATCH: usize = 256;
const MAX_GRPC_RECV_MSG_LEN: i32 = 10 * 1024 * 1024;

pub struct Server<T: RaftStoreRouter + 'static, S: StoreAddrResolver + 'static, E: Engine> {
    env: Arc<Environment>,
    // Grpc server.
    grpc_server: GrpcServer,
    local_addr: SocketAddr,
    // Transport.
    trans: ServerTransport<T, S>,
    raft_router: T,
    // The kv storage.
    storage: Storage<E>,
    // For handling coprocessor requests.
    end_point_worker: Worker<EndPointTask<E>>,
    // For sending/receiving snapshots.
    snap_mgr: SnapManager,
    snap_worker: Worker<SnapTask>,
    cop_readpool: ReadPool<coprocessor::ReadPoolContext>,
}

impl<T: RaftStoreRouter, S: StoreAddrResolver + 'static, E: Engine> Server<T, S, E> {
    #[cfg_attr(feature = "cargo-clippy", allow(too_many_arguments))]
    pub fn new(
        cfg: &Arc<Config>,
        security_mgr: &Arc<SecurityManager>,
        storage: Storage<E>,
        // TODO: Remove once endpoint itself is passed to here.
        cop_readpool: ReadPool<coprocessor::ReadPoolContext>,
        raft_router: T,
        resolver: S,
        snap_mgr: SnapManager,
        debug_engines: Option<Engines>,
        import_service: Option<ImportSSTService<T>>,
    ) -> Result<Self> {
        let env = Arc::new(
            EnvBuilder::new()
                .cq_count(cfg.grpc_concurrency)
                .name_prefix(thd_name!("grpc-server"))
                .build(),
        );
        let raft_client = Arc::new(RwLock::new(RaftClient::new(
            Arc::clone(&env),
            Arc::clone(cfg),
            Arc::clone(security_mgr),
        )));
        let end_point_worker = WorkerBuilder::new("end-point-worker")
            .batch_size(DEFAULT_COPROCESSOR_BATCH)
            .create();
        let snap_worker = Worker::new("snap-handler");

        let kv_service = KvService::new(
            storage.clone(),
            end_point_worker.scheduler(),
            raft_router.clone(),
            snap_worker.scheduler(),
            cfg.end_point_recursion_limit,
            cfg.end_point_stream_channel_size,
        );
        let addr = SocketAddr::from_str(&cfg.addr)?;
        info!("listening on {}", addr);
        let ip = format!("{}", addr.ip());
        let channel_args = ChannelBuilder::new(Arc::clone(&env))
            .stream_initial_window_size(cfg.grpc_stream_initial_window_size.0 as i32)
            .max_concurrent_stream(cfg.grpc_concurrent_stream)
            .max_receive_message_len(MAX_GRPC_RECV_MSG_LEN)
            .max_send_message_len(-1)
            .build_args();
        let grpc_server = {
            let mut sb = ServerBuilder::new(Arc::clone(&env))
                .channel_args(channel_args)
                .register_service(create_tikv(kv_service));
            sb = security_mgr.bind(sb, &ip, addr.port());
            if let Some(engines) = debug_engines {
                let debug_service = DebugService::new(engines, raft_router.clone());
                sb = sb.register_service(create_debug(debug_service));
            }
            if let Some(service) = import_service {
                sb = sb.register_service(create_import_sst(service));
            }
            sb.build()?
        };

        let addr = {
            let (ref host, port) = grpc_server.bind_addrs()[0];
            SocketAddr::new(IpAddr::from_str(host)?, port as u16)
        };

        let trans = ServerTransport::new(
            raft_client,
            snap_worker.scheduler(),
            raft_router.clone(),
            resolver,
        );

        let svr = Server {
            env: Arc::clone(&env),
            grpc_server,
            local_addr: addr,
            trans,
            raft_router,
            storage,
            end_point_worker,
            snap_mgr,
            snap_worker,
            cop_readpool,
        };

        Ok(svr)
    }

    pub fn transport(&self) -> ServerTransport<T, S> {
        self.trans.clone()
    }

    pub fn start(&mut self, cfg: Arc<Config>, security_mgr: Arc<SecurityManager>) -> Result<()> {
        let end_point = EndPointHost::new(
            self.storage.get_engine(),
            self.end_point_worker.scheduler(),
            &cfg,
            self.cop_readpool.clone(),
        );
        box_try!(self.end_point_worker.start(end_point));
        let snap_runner = SnapHandler::new(
            Arc::clone(&self.env),
            self.snap_mgr.clone(),
            self.raft_router.clone(),
            security_mgr,
            Arc::clone(&cfg),
        );
        box_try!(self.snap_worker.start(snap_runner));
        self.grpc_server.start();
        info!("TiKV is ready to serve");
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        self.end_point_worker.stop();
        self.snap_worker.stop();
        if let Err(e) = self.storage.stop() {
            error!("failed to stop store: {:?}", e);
        }
        self.grpc_server.shutdown();
        Ok(())
    }

    // Return listening address, this may only be used for outer test
    // to get the real address because we may use "127.0.0.1:0"
    // in test to avoid port conflict.
    pub fn listening_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::*;
    use std::sync::*;
    use std::time::Duration;

    use super::*;

    use super::super::resolve::{Callback as ResolveCallback, StoreAddrResolver};
    use super::super::transport::RaftStoreRouter;
    use super::super::{Config, Result};
    use kvproto::metapb::RegionEpoch;
    use kvproto::raft_cmdpb::RaftCmdRequest;
    use kvproto::raft_serverpb::RaftMessage;
    use raftstore::store::transport::Transport;
    use raftstore::store::*;
    use raftstore::Result as RaftStoreResult;
    use server::readpool::{self, ReadPool};
    use storage::{self, Config as StorageConfig, Storage};
    use util::mpsc::loose_bounded;
    use util::security::SecurityConfig;
    use util::worker::FutureWorker;

    #[derive(Clone)]
    struct MockResolver {
        quick_fail: Arc<AtomicBool>,
        addr: Arc<Mutex<Option<String>>>,
    }

    impl StoreAddrResolver for MockResolver {
        fn resolve(&self, _: u64, cb: ResolveCallback) -> Result<()> {
            if self.quick_fail.load(Ordering::SeqCst) {
                return Err(box_err!("quick fail"));
            }
            let addr = self.addr.lock().unwrap();
            cb(addr
                .as_ref()
                .map(|s| s.to_owned())
                .ok_or(box_err!("not set")));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct TestRaftStoreRouter {
        router: Router,
    }

    impl RaftStoreRouter for TestRaftStoreRouter {
        /// Send RaftMessage to local store.
        fn send_raft_msg(&self, msg: RaftMessage) -> RaftStoreResult<()> {
            self.router.send_raft_message(msg).unwrap();
            Ok(())
        }

        /// Send RaftCmdRequest to local store.
        fn send_command(&self, req: RaftCmdRequest, cb: Callback) -> RaftStoreResult<()> {
            self.router.send_cmd(req, cb).unwrap();
            Ok(())
        }

        /// Send a batch of RaftCmdRequests to local store.
        fn send_batch_commands(
            &self,
            _: Vec<RaftCmdRequest>,
            _: BatchReadCallback,
        ) -> RaftStoreResult<()> {
            unimplemented!()
        }

        fn async_split(
            &self,
            _: u64,
            _: RegionEpoch,
            _: Vec<Vec<u8>>,
            _: Callback,
        ) -> RaftStoreResult<()> {
            unimplemented!()
        }

        /// Send significant message. We should guarantee that the message can't be dropped.
        fn significant_send(&self, region_id: u64, msg: SignificantMsg) -> RaftStoreResult<()> {
            self.router
                .send_peer_message(region_id, PeerMsg::SignificantMsg(msg))
                .unwrap();
            Ok(())
        }
    }

    fn is_unreachable_to(msg: &PeerMsg, to_peer_id: u64) -> bool {
        match msg {
            PeerMsg::SignificantMsg(ref msg) => *msg == SignificantMsg::Unreachable { to_peer_id },
            _ => false,
        }
    }

    #[test]
    // if this failed, unset the environmental variables 'http_proxy' and 'https_proxy', and retry.
    fn test_peer_resolve() {
        let mut cfg = Config::default();
        let storage_cfg = StorageConfig::default();
        cfg.addr = "127.0.0.1:0".to_owned();

        let pd_worker = FutureWorker::new("test-future-worker");
        let storage_read_pool = ReadPool::new(
            "storage-readpool",
            &readpool::Config::default_for_test(),
            || || storage::ReadPoolContext::new(pd_worker.scheduler()),
        );
        let mut storage = Storage::new(&storage_cfg, storage_read_pool).unwrap();
        storage.start(&storage_cfg).unwrap();

        let (tx, rx) = Router::new_for_test(1);
        let (tx2, rx2) = loose_bounded(10);
        tx.register_mailbox(2, tx2);
        let router = TestRaftStoreRouter { router: tx };

        let addr = Arc::new(Mutex::new(None));
        let quick_fail = Arc::new(AtomicBool::new(false));
        let cfg = Arc::new(cfg);
        let security_mgr = Arc::new(SecurityManager::new(&SecurityConfig::default()).unwrap());

        let pd_worker = FutureWorker::new("test-pd-worker");
        let cop_read_pool = ReadPool::new(
            "cop-readpool",
            &readpool::Config::default_for_test(),
            || || coprocessor::ReadPoolContext::new(pd_worker.scheduler()),
        );

        let mut server = Server::new(
            &cfg,
            &security_mgr,
            storage,
            cop_read_pool,
            router.clone(),
            MockResolver {
                quick_fail: Arc::clone(&quick_fail),
                addr: Arc::clone(&addr),
            },
            SnapManager::new("", router.router),
            None,
            None,
        ).unwrap();

        server.start(cfg, security_mgr).unwrap();

        let mut trans = server.transport();
        let mut msg = RaftMessage::new();
        msg.set_region_id(1);
        msg.mut_to_peer().set_id(2);
        msg.mut_to_peer().set_store_id(1);
        trans.report_unreachable(msg);
        let mut resp = rx.try_recv().unwrap();
        assert!(is_unreachable_to(&resp, 2), "{:?}", resp);

        let mut msg = RaftMessage::new();
        msg.set_region_id(1);
        trans.send(msg.clone()).unwrap();
        trans.flush();
        resp = rx.try_recv().unwrap();
        assert!(is_unreachable_to(&resp, 0), "{:?}", resp);

        *addr.lock().unwrap() = Some(format!("{}", server.listening_addr()));

        trans.send(msg.clone()).unwrap();
        trans.flush();
        resp = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        match resp {
            PeerMsg::RaftMessage(m) => assert_eq!(m, msg),
            _ => panic!("raft message expected, but got {:?}", resp),
        }

        msg.mut_to_peer().set_store_id(2);
        msg.mut_to_peer().set_id(5);
        msg.set_region_id(2);
        assert!(rx2.try_recv().is_err());
        quick_fail.store(true, Ordering::SeqCst);
        trans.send(msg.clone()).unwrap();
        trans.flush();
        resp = rx2.try_recv().unwrap();
        assert!(is_unreachable_to(&resp, 5), "{:?}", resp);
        server.stop().unwrap();
    }
}
