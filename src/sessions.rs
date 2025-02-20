//
// Session Tracking
//
// This file is used to implement session tacking for the detector. There are a
// few specifics be to aware of if you are going to modify this file. 
//
// Current tracking is done as a Map of string to u64. The string is a
// derived from IP addresses of flows so that lookups can be performed quickly
// when we need to determine whether a flow is associated with a session. The
// u64 value is the timeout for the session which are periodically cleaned up by
// the FlowTracker that (currently) instantiates this.
//
// Notes:
//  - The timeout for flows can be updated. This exists for two reasons. 
//      1. if a connection exists when the timeout comes due the rule needs to
//         remain in effect until the connection is closed so that packets
//         continue to be forwarded over the DNAT tun interfaces.
//      2. If a second session is received which maps to the same key string and
//         has a longer timeout we nee to update the session to be valid until
//         the timeout of the longer session. Keep in mind that if a new
//         registration is received that has a shorter timeout we still need to
//         keep the longer timeout. 
//
// - The key strings that are matched against are currently different for ipv4
//   and ipv6, in v4 the string is a concatenation of the source and the
//   destination (client and phantom) addresses. In ipv6 it is only the phantom
//   address as the chance of phantom collisions is far lower.
//      * While not currently in use we could add the destination (phantom) port
//        to the key strings if we need extra specificity. 
//
// - The ingest thread is launched as a subroutine of the SessionTracker struct
//   and pulls from redis. The messages received come in the form of
//   StationToDetector protobuf, which can be modified relatively independently.
//   Currently there is a `from` function that parses this into SessionDetails
//   which can be directly managed by the SessionTracker.
//
// The notes above are implemented and tested below. If you modify the code
// please make sure the tests still pass. If you modify the way this code is
// used please update the tests. 

use std::collections::{HashMap};
use std::convert::From;
use std::fmt;
use std::net::{IpAddr};
use std::sync::{RwLock, Arc};
use std::thread;

use time::precise_time_ns;
use redis;

use signalling::StationToDetector;
use protobuf::Message;
use flow_tracker::{FlowNoSrcPort,FLOW_CLIENT_LOG};


const S2NS: u64= 1000*1000*1000;
// time to add beyond original timeout if a session is still receiving packets
// that need to be forwarded to the data plane proxying logic. (300 s = 5 mins)
const TIMEOUT_PHANTOMS_NS: u64 = 300 * S2NS;

// We _can_ filter by phantom port if we so choose, and randomize the port that
// the clients connect to. However we are currently using exclusively port 443.
// adding this here as a placeholder for now. 
const DEFAULT_PHANTOM_PORT: u16 = 443;


// "errors" we want to catch
#[derive(Debug)]
pub enum SessionError {
    InvalidPhantom,
    InvalidClient,
    MixedV4V6Error,
}

pub type SessionResult = Result<SessionDetails, SessionError>; 


impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SessionError::InvalidClient => {
                write!(f, "Invalid client address")
            },
            SessionError::InvalidPhantom => {
                write!(f, "Invalid phantom address")
            },
            SessionError::MixedV4V6Error => {
                write!(f, "Client/Phantom v4/v6 mismatch")
            },
        }
    }
}

#[derive(Copy, Clone)]
pub struct SessionDetails
{
    pub client_ip: IpAddr,
    pub phantom_ip: IpAddr,
    pub phantom_port: u32,
    timeout: u64,
}


impl SessionDetails
{
    // This function parses acceptable Session Details and returns an error if
    // the details provided do not fit current requirements for parsing 
    pub fn new(client_ip: &str, phantom_ip: &str, phantom_port: u32, timeout: u64) -> SessionResult {
        let phantom: IpAddr = match phantom_ip.parse() {
            Ok(ip) => ip,
            Err(_) => {return Err(SessionError::InvalidPhantom)},
        };

        let src: IpAddr = match client_ip.parse() {
            Ok(ip) => ip,
            Err(_) => {
                if client_ip == "" && phantom.is_ipv6() {
                    "::1".parse().unwrap()
                } else {
                    return Err(SessionError::InvalidClient)
                }
            },
        };

        if phantom.is_ipv4() && !src.is_ipv4() {
            return Err(SessionError::MixedV4V6Error)
        }

        let s = SessionDetails {
            client_ip: src,
            phantom_ip: phantom,
            phantom_port: phantom_port, //TODO: change u32 to u16 or add error catching
            timeout: timeout,
        };
        Ok(s)
    }

    pub fn get_key(&self) -> String {
        match self.phantom_ip.is_ipv6() {
            true => format!("{}-{}", self.phantom_ip, self.phantom_port),
            false => format!("{}-{}-{}", self.client_ip, self.phantom_ip, self.phantom_port)
        }
    }
}

impl From<&StationToDetector> for SessionResult {
    fn from(s2d: &StationToDetector) -> Self {
        let source = s2d.get_client_ip();
        let phantom = s2d.get_phantom_ip();
        let phantom_port = s2d.get_phantom_port();
        return SessionDetails::new(source, phantom, phantom_port, s2d.get_timeout_ns())
    }
}

// TODO - make accessible
impl fmt::Display for SessionDetails {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        unsafe {
            match FLOW_CLIENT_LOG {
                true => write!(f, "{} -> {}:{} ({}ns)", self.client_ip.to_string(), self.phantom_ip.to_string(), self.phantom_port.to_string(), self.timeout),
                false => write!(f, "_ -> {}:{} ({}ns)", self.phantom_ip.to_string(), self.phantom_port.to_string(), self.timeout),
            }
        }
    }
}

pub struct SessionTracker
{
    // Sessions cannot be tracked by registration because we will not be
    // receiving registration information in order to identify the sessions. As
    // such sessions are stored as a thread safe map with keys dependent on the
    // ip version:
    // v4 "{}-{}-{}", client_ip, phantom_ip, phantom_port
    // v6 "{}-{}", phantom_ip, phantom_port
    // TODO: ADDITION OF PORT IS WIP
    // The value stored for each of these is a timestamp to compare for timeout.
    pub tracked_sessions: Arc<RwLock<HashMap<String, u64>>>,
}

impl<'a> SessionTracker 
{
    pub fn new() -> SessionTracker {
        SessionTracker{
            tracked_sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn add_session(&mut self, det: SessionDetails) {
        self.insert_session(det)
    }

    pub fn spawn_update_thread(&self) {
        let write_map = Arc::clone(&self.tracked_sessions);
        thread::spawn(move || { ingest_from_pubsub(write_map) });
    }

    pub fn is_tracked_session(&self, flow: &FlowNoSrcPort) -> bool {
        let key = match flow.dst_ip.is_ipv6() {
            true => format!("{}-{}", flow.dst_ip, flow.dst_port),
            false => format!("{}-{}-{}", flow.src_ip, flow.dst_ip, flow.dst_port)
        };
        self.session_exists(&key)
    }

    pub fn len(&self) -> usize {
        let map = self.tracked_sessions.read().expect("RwLock Broken");
        let res = map.len();
        drop(map);
        return res
    }

    pub fn drop_stale_sessions(&mut self) -> usize {
        let right_now = precise_time_ns();

        let mut map = self.tracked_sessions.write().expect("RwLock Broken");
        let num_sessions_before = map.len();
        // Dark Decoys Map is not sorted by timeout, so need to check all
        map.retain(|_, v| ( *v > right_now));
        let num_sessions_after = map.len();
        if num_sessions_before != num_sessions_after {
            debug!("Dark Decoys drops: {} - > {}", num_sessions_before, num_sessions_after);
        }
        num_sessions_before - num_sessions_after
    }

    /// Used to update (increase) the time that we  consider a session 
    /// valid for tracking purposes. Called when packets from a session are
    /// seen so that forwarding continues past the original registration timeout.
    pub fn update_session(&mut self, flow: &FlowNoSrcPort) {

        let key = match flow.dst_ip.is_ipv6() {
            true => format!("{}-{}", flow.dst_ip, flow.dst_port),
            false => format!("{}-{}-{}", flow.src_ip, flow.dst_ip, flow.dst_port)
        };

        if !self.session_exists(&key) {
            return
        }

        self.try_update_session_timeout(key, TIMEOUT_PHANTOMS_NS);
    }

   
    
    fn try_update_session_timeout(&mut self, key: String, extra_time: u64) {
        // Get writable map
        let mut mmap = self.tracked_sessions.write().expect("RwLock broken");

        // Set timeout
        let expire_time = precise_time_ns() + extra_time;

        // compare and keep the longer
        match mmap.get_mut(&key){
            Some(v)=> {
                // compare and keep the longer
                if *v < expire_time {
                    *v = expire_time;
                }
            },
            None => {},
        };
    }

    fn insert_session(&mut self, session: SessionDetails) {
        // is this already in the map? 
        let key = session.get_key();
        if self.session_exists(&key) {
            self.try_update_session_timeout(key, session.timeout);
            return
        }

        // Get writable map
        let mut mmap = self.tracked_sessions.write().expect("RwLock broken");

        // Set timeout
        let expire_time = precise_time_ns() + session.timeout;

        // Insert
        *mmap.entry(key).or_insert(expire_time) = expire_time;

        // Get rid of writable reference to map.
        drop(mmap);

        debug!("Added registered ip {} from redis", session);
    }

    // explicitly used for testing
    fn _delete_session(&mut self, session: SessionDetails) {
        let key = &session.get_key();
        if ! self.session_exists(key) {
            return
        }
        let mut mmap = self.tracked_sessions.write().expect("RwLock broken");
        mmap.remove(key);
        // mmap.retain(|_, v| ( v.client_ip != session.client_ip || v.phantom_ip != session.phantom_ip));
    }

    // lookup session by identifier
    fn session_exists(&self, id: &String) -> bool
    { 
        let rmap = self.tracked_sessions.read().expect("RwLock broken");
        let res = rmap.contains_key(id);
        drop(rmap);
        return res
     }


}

// No returns in this function so that it runs for the lifetime of the process.
fn ingest_from_pubsub(map: Arc<RwLock<HashMap<String, u64>>>) {
    let mut con = get_redis_conn();
    let mut pubsub = con.as_pubsub();
    pubsub.subscribe("dark_decoy_map").expect("Can't subscribe to Redis");

    loop {
        let msg = match pubsub.get_message(){
            Ok(m) => m,
            Err(e) => {
                debug!("Error reading message from redis: {}", e);
                continue
            }
        };
        let payload : Vec<u8> = match msg.get_payload(){
            Ok(m) => m,
            Err(e) => {
                debug!("Error reading payload: {}", e);
                continue
            }
        };
        let station_to_det: StationToDetector = match Message::parse_from_bytes::<>(&payload) {
            Ok(s2d) => s2d,
            Err(e) => {
                debug!("failed to parse StationToDetector message {}", e);
                continue
            },
        };
        let sd = match SessionResult::from(&station_to_det){
            Ok(m) => m,
            Err(e) => {
                debug!("Error converting S2D to SD: {}", e);
                continue
            }
        };

        // is this already in the map? 
        let key = sd.get_key();
        // Get writable map
        let mut mmap = map.write().expect("RwLock broken");
        let exists = mmap.contains_key(&key);

        if exists {
            // Set timeout
            let expire_time = precise_time_ns() + sd.timeout;

            match mmap.get_mut(&key){
                Some(v)=> {
                    // compare and keep the longer
                    if *v < expire_time {
                        *v = expire_time;
                    }
                },
                None => {},
            };

            // Explicitly drop map write lock here (locks are automatically dropped
            // when they fall out of scope but this is more clear.)
            drop(mmap);
            continue
        }

        // Set timeout
        let expire_time = precise_time_ns() + sd.timeout;

        // Insert
        *mmap.entry(key).or_insert(expire_time) = expire_time;

        // Get rid of writable reference to map. (locks are automatically dropped
        // when they fall out of scope but this is more clear.)
        drop(mmap);

        debug!("Added registered ip {} from redis", sd);
    }
}

fn get_redis_conn() -> redis::Connection
{
    let client = redis::Client::open("redis://127.0.0.1/").expect("Can't open Redis");
    let con = client.get_connection().expect("Can't get Redis connection");
    con
}


#[cfg(test)]
mod tests {
    // use std::fmt::Write;
    use sessions::*;
    use signalling::StationToDetector;
    use flow_tracker::FlowNoSrcPort;
    use std::{thread, time};

    #[test]
    fn test_session_tracker_pubsub(){
        // // Publish to redis
        // let octs = match flow.dst_ip {
        //     IpAddr::V4(a) => a.octets().to_vec(),
        //     IpAddr::V6(a) => a.octets().to_vec(),
        // };
        // redis::cmd("PUBLISH").arg("dark_decoy_map").arg(octs).execute(&self.redis_conn);
        let st = SessionTracker::new();

        let test_tuples = [
            // (client_ip, phantom_ip, timeout)
            ("172.128.0.2", "8.0.0.1", 1),            // timeout immediately
            ("192.168.0.1", "10.10.0.1", 5*S2NS),
            ("192.168.0.1", "192.0.0.127", 5*S2NS),   
            ("", "2345::6789", 5*S2NS),
            
            // duplicate with shorter timeout should not drop
            ("2601::123:abcd", "2001::1234", 5*S2NS),
            ("::1", "2001::1234", 1*S2NS),
            
            // duplicate with long timeout should prevent drop
            ("7.0.0.2", "8.8.8.8", 1),
            ("7.0.0.2", "8.8.8.8", 5*S2NS),
        ];
    
        st.spawn_update_thread();
       
        let dur = time::Duration::new(3, 0);
        thread::sleep(dur);

        for entry in &test_tuples {
            let mut s2d = StationToDetector::new();
            s2d.set_client_ip(entry.0.to_string());
            s2d.set_phantom_ip(entry.1.to_string());
            s2d.set_timeout_ns(entry.2);

            let msg:Vec<u8> = s2d.write_to_bytes().unwrap();

            let redis_conn = get_redis_conn();
            redis::cmd("PUBLISH").arg("dark_decoy_map").arg(msg).execute(&redis_conn);
        }

        thread::sleep(dur);

        if st.len() != 6 {
            panic!("Failed to ingest from pubsub: {}", st.len());
        } 
    }


    #[test]
    fn test_session_details_from() {
        let test_tuples = [
            // (client_ip, phantom_ip, timeout)
            ("192.168.0.1", "10.10.0.1", 100000),
            ("2601::123:abcd", "2001::1234", 100000),
            ("", "2001::1234", 100000),
 
            // client registering with v4 will also create registrations for v6 just in-case
             ("192.168.0.1", "2801::1234", 100000),
        ];
        let test_tuples_bad = [
            // Mixed ipv4/ipv6 phantom/client
            ("2001::1234", "10.10.0.1", 100000, SessionError::MixedV4V6Error),

            // no phantom provided
            ("192.168.0.1", "", 100000, SessionError::InvalidPhantom),
            ("2601::123:abcd", "", 100000, SessionError::InvalidPhantom),

            // malformed addresses
            ("192.1", "10.0.0.1", 100000, SessionError::InvalidClient),
            ("2001::1234", "abcd::123::wrong", 100000, SessionError::InvalidPhantom),

            // No client provided in ipv4
            ("", "10.10.0.1", 100000, SessionError::InvalidClient),
        ];


        for entry in &test_tuples {
            let mut s2d = StationToDetector::new();
            s2d.set_client_ip(entry.0.to_string());
            s2d.set_phantom_ip(entry.1.to_string());
            s2d.set_timeout_ns(entry.2);
    
            let sd = match SessionResult::from(&s2d) {
                Ok(sd) => sd,
                Err(e) => {
                    panic!("Failed to parse StationToDetector: {}, {}", e, s2d.get_client_ip());
                }
            };
    
            // assert_eq!(entry.0, sd.client_ip.to_string());
            assert_eq!(entry.1, sd.phantom_ip.to_string());
            assert_eq!(entry.2, sd.timeout)
        }

        for entry in &test_tuples_bad {
            let mut s2d = StationToDetector::new();
            s2d.set_client_ip(entry.0.to_string());
            s2d.set_phantom_ip(entry.1.to_string());
            s2d.set_timeout_ns(entry.2);
    
            match SessionResult::from(&s2d) {
                Ok(_) => {
                    panic!("Should have failed");
                },
                Err(e) => {
                    assert_eq!(format!("{}",e), format!("{}",entry.3));
                },
            };
        }
    }

    #[test]
    fn test_session_tracker_basics() {

        let mut st = SessionTracker::new();

        let test_tuples = [
            // (client_ip, phantom_ip, timeout)
            ("192.168.0.1", "10.10.0.1", 443, 100000),
            ("192.168.0.1", "192.0.0.127", 443, 100000),     // duplicate client_addr
            ("2601::123:abcd", "2001::1234", 443, 100000),
            ("", "2001::1234", 443, 100000),                 // duplicate phantom Addr
            ("172.128.0.2", "8.0.0.1", 443, 1),              // timeout immediately
            
            // client registering with v4 will also create registrations for v6 just in-case
            ("192.168.0.1", "2801::1234", 100000, 443),
        ];

        for entry in &test_tuples {
            let s1 = SessionDetails::new(entry.0, entry.1, entry.2, entry.3).unwrap();
            st.insert_session(s1);
        }

        if st.len() != 5 {
            panic!("Either len is not working or insert is broken")
        };

        for entry in &test_tuples {
            let src = match entry.0 {
                "" => "::1".parse().unwrap(),
                _ => entry.0.parse().unwrap(),
            };
            let f = &FlowNoSrcPort{
                src_ip: src,
                dst_ip: entry.1.parse().unwrap(), 
                dst_port: DEFAULT_PHANTOM_PORT,
            };
            if !st.is_tracked_session(f) {
                panic!("Session should be tracked")
            }
        }

        let tt = test_tuples[0];
        let sd = SessionDetails::new(tt.0, tt.1, tt.2, tt.3).unwrap();
        st._delete_session(sd);


        if st.len() != 4 {
            panic!("Either len is not working or delete is broken")
        };
    }

    #[test]
    fn test_session_tracker_timeouts() {
        let mut st = SessionTracker::new();

        let test_tuples = [
            // (client_ip, phantom_ip, phantom_port, timeout)
            ("172.128.0.2", "8.0.0.1", 443, 1, false),            // timeout immediately
            ("192.168.0.1", "10.10.0.1", 443, 5*S2NS, true),
            ("192.168.0.1", "192.0.0.127", 443, 5*S2NS, true),    
 
            // client registering with v4 will also create registrations for v6 just in-case
             ("192.168.0.1", "2801::1234", 443, 5*S2NS, true),
            
            // duplicate with shorter timeout should not drop
            ("2601::123:abcd", "2001::1234", 443, 5*S2NS, true),
            ("::1", "2001::1234", 443, 1*S2NS, true),
            
            // duplicate with long timeout should prevent drop
            ("7.0.0.2", "8.8.8.8", 443, 1, true),
            ("7.0.0.2", "8.8.8.8", 443, 5*S2NS, true),
        ];
    
        for entry in &test_tuples {
            let s1 = SessionDetails::new(entry.0, entry.1, entry.2, entry.3).unwrap();
            st.insert_session(s1);
        }

        let dur = time::Duration::new(3, 0);
        thread::sleep(dur);

        assert_eq!(st.drop_stale_sessions(), 1);

        for entry in &test_tuples {
            let f = &FlowNoSrcPort{
                src_ip: entry.0.parse().unwrap(),
                dst_ip: entry.1.parse().unwrap(), 
                dst_port: DEFAULT_PHANTOM_PORT,
            };
            assert_eq!(st.is_tracked_session(f), entry.3)
        }

        thread::sleep(dur);
        
        assert_eq!(st.drop_stale_sessions(), 5);
    }
}