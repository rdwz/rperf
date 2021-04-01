use std::error::Error;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration};

use chashmap::CHashMap;

use clap::ArgMatches;

use mio::net::{TcpListener, TcpStream};
use mio::{Events, Ready, Poll, PollOpt, Token};

use crate::protocol::communication::{receive, send, KEEPALIVE_DURATION};

use crate::protocol::messaging::{
    prepare_connect, prepare_connect_ready,
};

use crate::stream::TestStream;
use crate::stream::tcp;
use crate::stream::udp;

type BoxResult<T> = Result<T,Box<dyn Error>>;

const POLL_TIMEOUT:Duration = Duration::from_millis(500);

/// when false, the system is shutting down
static ALIVE:AtomicBool = AtomicBool::new(true);

lazy_static::lazy_static!{
    /// a means of keeping track of which clients are running tests
    static ref CLIENTS:CHashMap<String, bool> = {
        let hm = CHashMap::new();
        hm
    };
}


fn handle_client(stream:&mut TcpStream, ip_version:&u8, cpu_affinity_manager:Arc<Mutex<crate::utils::cpu_affinity::CpuAffinityManager>>) -> BoxResult<()> {
    let mut started = false;
    let peer_addr = stream.peer_addr()?;
    
    
    //scaffolding to track and relay the streams and stream-results associated with this client
    let mut parallel_streams:Vec<Arc<Mutex<(dyn TestStream + Sync + Send)>>> = Vec::new();
    let mut parallel_streams_joinhandles = Vec::new();
    let (results_tx, results_rx):(std::sync::mpsc::Sender<Box<dyn crate::protocol::results::IntervalResult + Sync + Send>>, std::sync::mpsc::Receiver<Box<dyn crate::protocol::results::IntervalResult + Sync + Send>>) = channel();
    
    //a closure used to pass results from stream-handlers to the client-communication stream
    let mut forwarding_send_stream = stream.try_clone()?;
    let mut results_handler = || -> BoxResult<()> {
        loop { //drain all results every time this closer is invoked
            match results_rx.try_recv() { //if there's something to forward, write it to the client
                Ok(result) => {
                    send(&mut forwarding_send_stream, &result.to_json())?;
                },
                Err(_) => break, //whether it's empty or disconnected, there's nothing to do
            }
        }
        Ok(())
    };
    
    
    //server operations are entirely driven by client-signalling, making this a (simple) state-machine
    while is_alive() {
        let payload = receive(stream, is_alive, &mut results_handler)?;
        match payload.get("kind") {
            Some(kind) => {
                match kind.as_str().unwrap() {
                    "configuration" => { //we either need to connect streams to the client or prepare to receive connections
                        if payload.get("role").unwrap_or(&serde_json::json!("download")).as_str().unwrap() == "download" {
                            log::debug!("[{}] running in forward-mode: server will be receiving data", &peer_addr);
                            
                            let stream_count = payload.get("streams").unwrap_or(&serde_json::json!(1)).as_i64().unwrap();
                            //since we're receiving data, we're also responsible for letting the client know where to send it
                            let mut stream_ports = Vec::with_capacity(stream_count as usize);
                            
                            if payload.get("family").unwrap_or(&serde_json::json!("tcp")).as_str().unwrap() == "udp" {
                                log::info!("[{}] preparing for UDP test with {} streams...", &peer_addr, stream_count);
                                
                                let test_definition = udp::UdpTestDefinition::new(&payload)?;
                                for stream_idx in 0..stream_count {
                                    log::debug!("[{}] preparing UDP-receiver for stream {}...", &peer_addr, stream_idx);
                                    let test = udp::receiver::UdpReceiver::new(
                                        test_definition.clone(), &(stream_idx as u8),
                                        &ip_version, &0,
                                        &(payload["receiveBuffer"].as_i64().unwrap() as usize),
                                    )?;
                                    stream_ports.push(test.get_port()?);
                                    parallel_streams.push(Arc::new(Mutex::new(test)));
                                }
                            } else { //TCP
                                log::info!("[{}] preparing for TCP test with {} streams...", &peer_addr, stream_count);
                                
                                let test_definition = tcp::TcpTestDefinition::new(&payload)?;
                                for stream_idx in 0..stream_count {
                                    log::debug!("[{}] preparing TCP-receiver for stream {}...", &peer_addr, stream_idx);
                                    let test = tcp::receiver::TcpReceiver::new(
                                        test_definition.clone(), &(stream_idx as u8),
                                        &ip_version, &0,
                                        &(payload["receiveBuffer"].as_i64().unwrap() as usize),
                                    )?;
                                    stream_ports.push(test.get_port()?);
                                    parallel_streams.push(Arc::new(Mutex::new(test)));
                                }
                            }
                            
                            //let the client know we're ready to receive the connection; stream-ports are in stream-index order
                            send(stream, &prepare_connect(&stream_ports))?;
                        } else { //upload
                            log::debug!("[{}] running in reverse-mode: server will be uploading data", &peer_addr);
                            
                            let stream_ports = payload.get("streamPorts").unwrap().as_array().unwrap();
                            
                            if payload.get("family").unwrap_or(&serde_json::json!("tcp")).as_str().unwrap() == "udp" {
                                log::info!("[{}] preparing for UDP test with {} streams...", &peer_addr, stream_ports.len());
                                
                                let test_definition = udp::UdpTestDefinition::new(&payload)?;
                                for (stream_idx, port) in stream_ports.iter().enumerate() {
                                    log::debug!("[{}] preparing UDP-sender for stream {}...", &peer_addr, stream_idx);
                                    let test = udp::sender::UdpSender::new(
                                        test_definition.clone(), &(stream_idx as u8),
                                        &ip_version, &0, peer_addr.ip().to_string(), &(port.as_i64().unwrap_or(0) as u16),
                                        &(payload.get("duration").unwrap_or(&serde_json::json!(0.0)).as_f64().unwrap() as f32),
                                        &(payload.get("sendInterval").unwrap_or(&serde_json::json!(1.0)).as_f64().unwrap() as f32),
                                        &(payload["sendBuffer"].as_i64().unwrap() as usize),
                                    )?;
                                    parallel_streams.push(Arc::new(Mutex::new(test)));
                                }
                            } else { //TCP
                                log::info!("[{}] preparing for TCP test with {} streams...", &peer_addr, stream_ports.len());
                                
                                let test_definition = tcp::TcpTestDefinition::new(&payload)?;
                                for (stream_idx, port) in stream_ports.iter().enumerate() {
                                    log::debug!("[{}] preparing TCP-sender for stream {}...", &peer_addr, stream_idx);
                                    let test = tcp::sender::TcpSender::new(
                                        test_definition.clone(), &(stream_idx as u8),
                                        &ip_version, peer_addr.ip().to_string(), &(port.as_i64().unwrap() as u16),
                                        &(payload["duration"].as_f64().unwrap() as f32),
                                        &(payload["sendInterval"].as_f64().unwrap() as f32),
                                        &(payload["sendBuffer"].as_i64().unwrap() as usize),
                                        &(payload["noDelay"].as_bool().unwrap()),
                                    )?;
                                    parallel_streams.push(Arc::new(Mutex::new(test)));
                                }
                            }
                            
                            //let the client know we're ready to begin
                            send(stream, &prepare_connect_ready())?;
                        }
                    },
                    "begin" => { //the client has indicated that testing can begin
                        if !started { //a simple guard to protect against reinitialisaion
                            for (stream_idx, parallel_stream) in parallel_streams.iter_mut().enumerate() {
                                log::info!("[{}] beginning execution of stream {}...", &peer_addr, stream_idx);
                                let c_ps = Arc::clone(&parallel_stream);
                                let c_results_tx = results_tx.clone();
                                let c_cam = cpu_affinity_manager.clone();
                                let handle = thread::spawn(move || {
                                    { //set CPU affinity, if enabled
                                        c_cam.lock().unwrap().set_affinity();
                                    }
                                    loop {
                                        let mut test = c_ps.lock().unwrap();
                                        log::debug!("[{}] beginning test-interval for stream {}", &peer_addr, test.get_idx());
                                        match test.run_interval() {
                                            Some(interval_result) => match interval_result {
                                                Ok(ir) => match c_results_tx.send(ir) {
                                                    Ok(_) => (),
                                                    Err(e) => {
                                                        log::error!("[{}] unable to process interval-result: {}", &peer_addr, e);
                                                        break
                                                    },
                                                },
                                                Err(e) => {
                                                    log::error!("[{}] unable to process stream: {}", peer_addr, e);
                                                    match c_results_tx.send(Box::new(crate::protocol::results::ServerFailedResult{stream_idx: test.get_idx()})) {
                                                        Ok(_) => (),
                                                        Err(e) => log::error!("[{}] unable to report interval-failed-result: {}", &peer_addr, e),
                                                    }
                                                    break;
                                                },
                                            },
                                            None => {
                                                match c_results_tx.send(Box::new(crate::protocol::results::ServerDoneResult{stream_idx: test.get_idx()})) {
                                                    Ok(_) => (),
                                                    Err(e) => log::error!("[{}] unable to report interval-done-result: {}", &peer_addr, e),
                                                }
                                                break;
                                            },
                                        }
                                    }
                                });
                                parallel_streams_joinhandles.push(handle);
                            }
                            started = true;
                        } else { //this can only happen in case of malicious action
                            log::error!("[{}] duplicate begin-signal", &peer_addr);
                            break;
                        }
                    },
                    "end" => { //the client has indicated that testing is done; stop cleanly
                        log::info!("[{}] end of testing signaled", &peer_addr);
                        break;
                    },
                    _ => {
                        log::error!("[{}] invalid data", &peer_addr);
                        break;
                    },
                }
            },
            None => {
                log::error!("[{}] invalid data", &peer_addr);
                break;
            },
        }
    }
    
    log::debug!("[{}] stopping any still-in-progress streams", &peer_addr);
    for ps in parallel_streams.iter_mut() {
        let mut stream = match (*ps).lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::error!("[{}] a stream-handler was poisoned; this indicates some sort of logic error", &peer_addr);
                poisoned.into_inner()
            },
        };
        stream.stop();
    }
    log::debug!("[{}] waiting for all streams to end", &peer_addr);
    for jh in parallel_streams_joinhandles {
        match jh.join() {
            Ok(_) => (),
            Err(e) => log::error!("[{}] error in parallel stream: {:?}", &peer_addr, e),
        }
    }
    
    Ok(())
}

pub fn serve(args:ArgMatches) -> BoxResult<()> {
    //config-parsing and pre-connection setup
    let cpu_affinity_manager = Arc::new(Mutex::new(crate::utils::cpu_affinity::CpuAffinityManager::new(args.value_of("affinity").unwrap())?));
    
    let ip_version:u8;
    if args.is_present("version6") {
        ip_version = 6;
    } else {
        ip_version = 4;
    }
    let port:u16 = args.value_of("port").unwrap().parse()?;
    
    
    //start listening for connections
    let mut listener:TcpListener;
    if ip_version == 4 {
        listener = TcpListener::bind(&SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)).expect(format!("failed to bind TCP socket, port {}", port).as_str());
    } else if ip_version == 6 {
        listener = TcpListener::bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).expect(format!("failed to bind TCP socket, port {}", port).as_str());
    } else {
        return Err(Box::new(simple_error::simple_error!("unsupported IP version: {}", ip_version)));
    }
    log::info!("server listening on port {}, IPv{}", port, ip_version);
    
    let mio_token = Token(0);
    let poll = Poll::new()?;
    poll.register(
        &mut listener,
        mio_token,
        Ready::readable(),
        PollOpt::edge(),
    )?;
    let mut events = Events::with_capacity(32);
    
    while is_alive() {
        poll.poll(&mut events, Some(POLL_TIMEOUT))?;
        for event in events.iter() {
            match event.token() {
                _ => loop {
                    match listener.accept() {
                        Ok((mut stream, address)) => {
                            log::info!("connection from {}", address);
                            
                            stream.set_nodelay(true).expect("cannot disable Nagle's algorithm");
                            stream.set_keepalive(Some(KEEPALIVE_DURATION)).expect("unable to set TCP keepalive");
                            
                            CLIENTS.insert(address.to_string(), false);
                            
                            let c_cam = cpu_affinity_manager.clone();
                            thread::spawn(move || {
                                match handle_client(&mut stream, &ip_version, c_cam) {
                                    Ok(_) => (),
                                    Err(e) => log::error!("error in client-handler: {}", e),
                                }
                                CLIENTS.remove(&address.to_string());
                                stream.shutdown(Shutdown::Both).unwrap_or_default();
                            });
                        },
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => { //nothing to do
                            break;
                        },
                        Err(e) => {
                            return Err(Box::new(e));
                        },
                    }
                },
            }
        }
    }
    
    //wait until all clients have been disconnected
    while CLIENTS.len() > 0 {
        log::info!("waiting for {} clients to finish...", CLIENTS.len());
        thread::sleep(POLL_TIMEOUT);
    }
    Ok(())
}

pub fn kill() -> bool {
    ALIVE.swap(false, Ordering::Relaxed)
}
fn is_alive() -> bool {
    ALIVE.load(Ordering::Relaxed)
}
