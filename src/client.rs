/*
 * Copyright (C) 2021 Evtech Solutions, Ltd., dba 3D-P
 * Copyright (C) 2021 Neil Tallim <neiltallim@3d-p.com>
 * 
 * ... (license text unchanged)
 */

 use std::net::{IpAddr, Shutdown, ToSocketAddrs};
 use std::sync::atomic::{AtomicBool, Ordering};
 use std::sync::{Arc, Mutex};
 use std::sync::mpsc::channel;
 use std::thread;
 use std::time::{Duration, SystemTime, UNIX_EPOCH};
 use std::error::Error;
 
 use clap::ArgMatches;
 use mio::net::TcpStream;
 
 use crate::protocol::communication::{receive, send, KEEPALIVE_DURATION};
 use crate::protocol::messaging::{prepare_begin, prepare_end, prepare_upload_configuration, prepare_download_configuration};
 use crate::protocol::results::{IntervalResult, IntervalResultKind, TestResults, TcpTestResults, UdpTestResults};
 use crate::stream::TestStream;
 use crate::stream::tcp;
 use crate::stream::udp;
 
 use anyhow::Result;
 
 type BoxResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
 
 static ALIVE: AtomicBool = AtomicBool::new(true);
 static mut KILL_TIMER_RELATIVE_START_TIME: f64 = 0.0;
 const KILL_TIMEOUT: f64 = 5.0;
 const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
 
 fn connect_to_server(address: &str, port: &u16) -> BoxResult<TcpStream> {
     let destination = format!("{}:{}", address, port);
     log::info!("connecting to server at {}...", destination);
     
     let server_addr = destination.to_socket_addrs()?
         .next()
         .ok_or_else(|| anyhow::anyhow!("unable to resolve {}", address))?;
     
     let raw_stream = std::net::TcpStream::connect_timeout(&server_addr, CONNECT_TIMEOUT)
         .map_err(|e| anyhow::anyhow!("unable to connect: {}", e))?;
     
     let stream = TcpStream::from_stream(raw_stream)
         .map_err(|e| anyhow::anyhow!("unable to prepare TCP control-channel: {}", e))?;
     
     log::info!("connected to server");
     
     stream.set_nodelay(true).expect("cannot disable Nagle's algorithm");
     stream.set_keepalive(Some(KEEPALIVE_DURATION)).expect("unable to set TCP keepalive");
     
     Ok(stream)
 }
 
 fn prepare_test_results(is_udp: bool, stream_count: u8) -> Mutex<Box<dyn TestResults>> {
     if is_udp {
         let mut udp_test_results = UdpTestResults::new();
         for i in 0..stream_count {
             udp_test_results.prepare_index(&i);
         }
         Mutex::new(Box::new(udp_test_results))
     } else {
         let mut tcp_test_results = TcpTestResults::new();
         for i in 0..stream_count {
             tcp_test_results.prepare_index(&i);
         }
         Mutex::new(Box::new(tcp_test_results))
     }
 }
 
 pub fn execute(args: ArgMatches, output: Arc<Mutex<Vec<u8>>>) -> BoxResult<()> {
     let mut complete = false;
     
     let mut tcp_port_pool = tcp::receiver::TcpPortPool::new(
         args.value_of("tcp_port_pool").unwrap().to_string(),
         args.value_of("tcp6_port_pool").unwrap().to_string(),
     );
     let mut udp_port_pool = udp::receiver::UdpPortPool::new(
         args.value_of("udp_port_pool").unwrap().to_string(),
         args.value_of("udp6_port_pool").unwrap().to_string(),
     );

     let cpu_affinity_manager = Arc::new(Mutex::new(crate::utils::cpu_affinity::CpuAffinityManager::new(args.value_of("affinity").unwrap())?));
     
     let display_json: bool;
     let display_bit: bool;
     match args.value_of("format").unwrap() {
         "json" => {
             display_json = true;
             display_bit = false;
         },
         "megabit" => {
             display_json = false;
             display_bit = true;
         },
         "megabyte" => {
             display_json = false;
             display_bit = false;
         },
         _ => {
             log::error!("unsupported display-mode; defaulting to JSON");
             display_json = true;
             display_bit = false;
         },
     }
     
     let is_udp = args.is_present("udp");
     
     let test_id = uuid::Uuid::new_v4();
     let mut upload_config = prepare_upload_configuration(&args, test_id.as_bytes())?;
     let mut download_config = prepare_download_configuration(&args, test_id.as_bytes())?;
     
     let mut stream = connect_to_server(&args.value_of("client").unwrap(), &(args.value_of("port").unwrap().parse()?))?;
     let server_addr = stream.peer_addr()?;
     
     let stream_count = download_config.get("streams").unwrap().as_i64().unwrap() as usize;
     let mut parallel_streams: Vec<Arc<Mutex<(dyn TestStream + Sync + Send)>>> = Vec::with_capacity(stream_count);
     let mut parallel_streams_joinhandles = Vec::with_capacity(stream_count);
     let (results_tx, results_rx): (std::sync::mpsc::Sender<Box<dyn IntervalResult + Sync + Send>>, std::sync::mpsc::Receiver<Box<dyn IntervalResult + Sync + Send>>) = channel();
     
     let test_results: Mutex<Box<dyn TestResults>> = prepare_test_results(is_udp, stream_count as u8);
     
     let mut results_handler = || -> BoxResult<()> {
         loop {
             match results_rx.try_recv() {
                 Ok(result) => {
                     if !display_json {
                         let output_str = format!("{}\n", result.to_string(display_bit));
                         output.lock().unwrap().extend_from_slice(output_str.as_bytes());
                     }
                     let mut tr = test_results.lock().unwrap();
                     match result.kind() {
                         IntervalResultKind::ClientDone | IntervalResultKind::ClientFailed => {
                             if result.kind() == IntervalResultKind::ClientDone {
                                 log::info!("stream {} is done", result.get_stream_idx());
                             } else {
                                 log::warn!("stream {} failed", result.get_stream_idx());
                             }
                             tr.mark_stream_done(&result.get_stream_idx(), result.kind() == IntervalResultKind::ClientDone);
                             if tr.count_in_progress_streams() == 0 {
                                 complete = true;
                                 if tr.count_in_progress_streams_server() > 0 {
                                     log::info!("giving the server a few seconds to report results...");
                                     start_kill_timer();
                                 } else {
                                     kill();
                                 }
                             }
                         },
                         _ => {
                             tr.update_from_json(result.to_json())?;
                         }
                     }
                 },
                 Err(_) => break,
             }
         }
         Ok(())
     };
     
     if args.is_present("reverse") {
         log::debug!("running in reverse-mode: server will be uploading data");
         
         let mut stream_ports = Vec::with_capacity(stream_count);
         
         if is_udp {
             log::info!("preparing for reverse-UDP test with {} streams...", stream_count);
             let test_definition = udp::UdpTestDefinition::new(&download_config)?;
             for stream_idx in 0..stream_count {
                 log::debug!("preparing UDP-receiver for stream {}...", stream_idx);
                 let test = udp::receiver::UdpReceiver::new(
                     test_definition.clone(), &(stream_idx as u8),
                     &mut udp_port_pool,
                     &server_addr.ip(),
                     &(download_config["receive_buffer"].as_i64().unwrap() as usize),
                 )?;
                 stream_ports.push(test.get_port()?);
                 parallel_streams.push(Arc::new(Mutex::new(test)));
             }
         } else {
             log::info!("preparing for reverse-TCP test with {} streams...", stream_count);
             let test_definition = tcp::TcpTestDefinition::new(&download_config)?;
             for stream_idx in 0..stream_count {
                 log::debug!("preparing TCP-receiver for stream {}...", stream_idx);
                 let test = tcp::receiver::TcpReceiver::new(
                     test_definition.clone(), &(stream_idx as u8),
                     &mut tcp_port_pool,
                     &server_addr.ip(),
                     &(download_config["receive_buffer"].as_i64().unwrap() as usize),
                 )?;
                 stream_ports.push(test.get_port()?);
                 parallel_streams.push(Arc::new(Mutex::new(test)));
             }
         }
         
         upload_config["stream_ports"] = serde_json::json!(stream_ports);
         send(&mut stream, &upload_config)?;
     } else {
         log::debug!("running in forward-mode: server will be receiving data");
         send(&mut stream, &download_config)?;
     }
     
     let connection_payload = receive(&mut stream, is_alive, &mut results_handler)?;
     match connection_payload.get("kind") {
         Some(kind) => {
             match kind.as_str().unwrap_or_default() {
                 "connect" => {
                     if is_udp {
                         log::info!("preparing for UDP test with {} streams...", stream_count);
                         let test_definition = udp::UdpTestDefinition::new(&upload_config)?;
                         for (stream_idx, port) in connection_payload.get("stream_ports").unwrap().as_array().unwrap().iter().enumerate() {
                             log::debug!("preparing UDP-sender for stream {}...", stream_idx);
                             let test = udp::sender::UdpSender::new(
                                 test_definition.clone(), &(stream_idx as u8),
                                 &0, &server_addr.ip(), &(port.as_i64().unwrap() as u16),
                                 &(upload_config["duration"].as_f64().unwrap() as f32),
                                 &(upload_config["send_interval"].as_f64().unwrap() as f32),
                                 &(upload_config["send_buffer"].as_i64().unwrap() as usize),
                             )?;
                             parallel_streams.push(Arc::new(Mutex::new(test)));
                         }
                     } else {
                         log::info!("preparing for TCP test with {} streams...", stream_count);
                         let test_definition = tcp::TcpTestDefinition::new(&upload_config)?;
                         for (stream_idx, port) in connection_payload.get("stream_ports").unwrap().as_array().unwrap().iter().enumerate() {
                             log::debug!("preparing TCP-sender for stream {}...", stream_idx);
                             let test = tcp::sender::TcpSender::new(
                                 test_definition.clone(), &(stream_idx as u8),
                                 &server_addr.ip(), &(port.as_i64().unwrap() as u16),
                                 &(upload_config["duration"].as_f64().unwrap() as f32),
                                 &(upload_config["send_interval"].as_f64().unwrap() as f32),
                                 &(upload_config["send_buffer"].as_i64().unwrap() as usize),
                                 &(upload_config["no_delay"].as_bool().unwrap()),
                             )?;
                             parallel_streams.push(Arc::new(Mutex::new(test)));
                         }
                     }
                 },
                 "connect-ready" => {},
                 _ => {
                     log::error!("invalid data from {}: {}", stream.peer_addr()?, serde_json::to_string(&connection_payload)?);
                     kill();
                 },
             }
         },
         None => {
             log::error!("invalid data from {}: {}", stream.peer_addr()?, serde_json::to_string(&connection_payload)?);
             kill();
         },
     }
     
     if is_alive() {
         log::info!("informing server that testing can begin...");
         send(&mut stream, &prepare_begin())?;
         
         log::debug!("spawning stream-threads");
         for (stream_idx, parallel_stream) in parallel_streams.iter_mut().enumerate() {
             log::info!("beginning execution of stream {}...", stream_idx);
             let c_ps = Arc::clone(parallel_stream);
             let c_results_tx = results_tx.clone();
             let c_cam = cpu_affinity_manager.clone();
             let handle = thread::spawn(move || {
                 c_cam.lock().unwrap().set_affinity();
                 loop {
                     let mut test = c_ps.lock().unwrap();
                     log::debug!("beginning test-interval for stream {}", test.get_idx());
                     match test.run_interval() {
                         Some(interval_result) => match interval_result {
                             Ok(ir) => match c_results_tx.send(ir) {
                                 Ok(_) => (),
                                 Err(e) => {
                                     log::error!("unable to report interval-result: {}", e);
                                     break;
                                 },
                             },
                             Err(e) => {
                                 log::error!("unable to process stream: {}", e);
                                 match c_results_tx.send(Box::new(crate::protocol::results::ClientFailedResult { stream_idx: test.get_idx() })) {
                                     Ok(_) => (),
                                     Err(e) => log::error!("unable to report interval-failed-result: {}", e),
                                 }
                                 break;
                             },
                         },
                         None => {
                             match c_results_tx.send(Box::new(crate::protocol::results::ClientDoneResult { stream_idx: test.get_idx() })) {
                                 Ok(_) => (),
                                 Err(e) => log::error!("unable to report interval-done-result: {}", e),
                             }
                             break;
                         },
                     }
                 }
             });
             parallel_streams_joinhandles.push(handle);
         }
         
         while is_alive() {
             match receive(&mut stream, is_alive, &mut results_handler) {
                 Ok(payload) => {
                     match payload.get("kind") {
                         Some(kind) => {
                             match kind.as_str().unwrap_or_default() {
                                 "receive" | "send" => {
                                     if !display_json {
                                         let result = crate::protocol::results::interval_result_from_json(payload.clone())?;
                                         let output_str = format!("{}\n", result.to_string(display_bit));
                                         output.lock().unwrap().extend_from_slice(output_str.as_bytes());
                                     }
                                     let mut tr = test_results.lock().unwrap();
                                     tr.update_from_json(payload)?;
                                 },
                                 "done" | "failed" => match payload.get("stream_idx") {
                                     Some(stream_idx) => match stream_idx.as_i64() {
                                         Some(idx64) => {
                                             let mut tr = test_results.lock().unwrap();
                                             match kind.as_str().unwrap() {
                                                 "done" => log::info!("server reported completion of stream {}", idx64),
                                                 "failed" => {
                                                     log::warn!("server reported failure with stream {}", idx64);
                                                     tr.mark_stream_done(&(idx64 as u8), false);
                                                 },
                                                 _ => (),
                                             }
                                             tr.mark_stream_done_server(&(idx64 as u8));
                                             if tr.count_in_progress_streams() == 0 && tr.count_in_progress_streams_server() == 0 {
                                                 kill();
                                             }
                                         },
                                         None => log::error!("completion from server did not include a valid stream_idx"),
                                     },
                                     None => log::error!("completion from server did not include stream_idx"),
                                 },
                                 _ => {
                                     log::error!("invalid data from {}: {}", stream.peer_addr()?, serde_json::to_string(&connection_payload)?);
                                     break;
                                 },
                             }
                         },
                         None => {
                             log::error!("invalid data from {}: {}", stream.peer_addr()?, serde_json::to_string(&connection_payload)?);
                             break;
                         },
                     }
                 },
                 Err(e) => {
                     if !complete {
                         return Err(e);
                     }
                     break;
                 },
             }
         }
     }
     
     send(&mut stream, &prepare_end()).unwrap_or_default();
     thread::sleep(Duration::from_millis(250));
     stream.shutdown(Shutdown::Both).unwrap_or_default();
     
     log::debug!("stopping any still-in-progress streams");
     for ps in parallel_streams.iter_mut() {
         let mut stream = match ps.lock() {
             Ok(guard) => guard,
             Err(poisoned) => {
                 log::error!("a stream-handler was poisoned; this indicates some sort of logic error");
                 poisoned.into_inner()
             },
         };
         stream.stop();
     }
     log::debug!("waiting for all streams to end");
     for jh in parallel_streams_joinhandles {
         match jh.join() {
             Ok(_) => (),
             Err(e) => log::error!("error in parallel stream: {:?}", e),
         }
     }
     
     let common_config: serde_json::Value;
     {
         let upload_config_map = upload_config.as_object_mut().unwrap();
         let cc_family = upload_config_map.remove("family");
         upload_config_map.remove("kind");
         let cc_length = upload_config_map.remove("length");
         upload_config_map.remove("role");
         let cc_streams = upload_config_map.remove("streams");
         upload_config_map.remove("test_id");
         upload_config_map.remove("stream_ports");
         if upload_config_map["send_buffer"].as_i64().unwrap() == 0 {
             upload_config_map.remove("send_buffer");
         }
         
         let download_config_map = download_config.as_object_mut().unwrap();
         download_config_map.remove("family");
         download_config_map.remove("kind");
         download_config_map.remove("length");
         download_config_map.remove("role");
         download_config_map.remove("streams");
         download_config_map.remove("test_id");
         if download_config_map["receive_buffer"].as_i64().unwrap() == 0 {
             download_config_map.remove("receive_buffer");
         }
         
         common_config = serde_json::json!({
             "family": cc_family,
             "length": cc_length,
             "streams": cc_streams,
         });
     }
     
     log::debug!("displaying test results");
     let omit_seconds: usize = args.value_of("omit").unwrap().parse()?;
     {
         let tr = test_results.lock().unwrap();
         let output_str = if display_json {
             tr.to_json_string(omit_seconds, upload_config, download_config, common_config, serde_json::json!({
                 "omit_seconds": omit_seconds,
                 "ip_version": match server_addr.ip() {
                     IpAddr::V4(_) => 4,
                     IpAddr::V6(_) => 6,
                 },
                 "reverse": args.is_present("reverse"),
             }))
         } else {
             format!("{}\n", tr.to_string(display_bit, omit_seconds))
         };
         output.lock().unwrap().extend_from_slice(output_str.as_bytes());
     }
     
     Ok(())
 }
 
 pub fn kill() -> bool {
     ALIVE.swap(false, Ordering::Relaxed)
 }
 
 fn start_kill_timer() {
     unsafe {
         KILL_TIMER_RELATIVE_START_TIME = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
     }
 }
 
 fn is_alive() -> bool {
     unsafe {
         if KILL_TIMER_RELATIVE_START_TIME != 0.0 {
             if SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() - KILL_TIMER_RELATIVE_START_TIME >= KILL_TIMEOUT {
                 return false;
             }
         }
     }
     ALIVE.load(Ordering::Relaxed)
 }