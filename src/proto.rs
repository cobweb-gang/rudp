#![allow(dead_code)] //////////// REMOVE REMOVE DEBUG DEBUG TODO TODO

use std::{
	collections::{HashMap, HashSet},
	io, fmt, time, iter, cmp,
	time::Instant,
	io::{
		// Read,
		Write,
	},
};
use mio::net::UdpSocket;

use rand::{thread_rng, Rng};
use byteorder::{ReadBytesExt, WriteBytesExt};
// use indexmap::IndexMap;
use mod_ord::ModOrd;
use mio::*;

/*
TODO
- organize imports
- log(n) means of scanning inbox
- io::Write interface for sending
- buffered written messages?
- acknowledgements and explicit RESEND_LOST
- encrypt message headers. 
- verify contents of incoming headers
--- ensure sequence numbers are all reasonable and within window
--- check a secret NONCE that is set by the user
- rudp server. takes bind address and 
*/

struct EndpointConfig {
	pub max_msg_size: usize,
	pub buffer_grow_space: usize,
	pub window_size: u32,
	// pub must_resend_func: Box<FnMut(u32, time::Duration) -> bool>,
}
impl EndpointConfig {
	fn default() -> Self {
		EndpointConfig {
			max_msg_size: 2048,
			buffer_grow_space: 1024,
			window_size: 64,
		}
	}
}


#[derive(Debug)]
struct Endpoint<U: UdpLike> {
	//both in and out
	socket: U,
	buf: Vec<u8>,
	buf_free_start: usize,
	max_yielded: ModOrd, // for acking


	//outgoing
	next_id: ModOrd,
	wait_until: ModOrd,
	 // only stores delivery messages
	outbox: HashMap<ModOrd, (Instant, *const [u8])>,
	outbox2: HashMap<ModOrd, (Instant, Vec<u8>)>,
	peer_acked: ModOrd,

	//incoming
	buf_min_space: usize,
	n: ModOrd,
	largest_set_id_yielded: ModOrd,
	seen_before: HashSet<ModOrd>, // contains messages only in THIS set
	inbox: HashMap<ModOrd, Message>,
	inbox2: HashMap<ModOrd, OwnedMessage>, 
	inbox2_to_remove: Option<ModOrd>,
	window_size: u32,
}

impl<U> Endpoint<U> where U: UdpLike {

	fn resend_lost(&mut self) -> io::Result<()> {
		let a = self.peer_acked;
		self.outbox.retain(|&id, _| id > a);
		self.outbox2.retain(|&id, _| id > a);
		let now = Instant::now();
		for (id, (ref mut instant, ref_bytes)) in self.outbox.iter_mut() {
			if Self::too_stale_func(id.abs_difference(self.n), instant.elapsed()) {
				//resend
				self.socket.send(unsafe{&**ref_bytes})?;
				*instant = now;
			}
		}
		for (id, (ref mut instant, ref vec)) in self.outbox2.iter_mut() {
			if Self::too_stale_func(id.abs_difference(self.n), instant.elapsed()) {
				//resend
				self.socket.send(&vec[..])?;
				*instant = now;
			}
		}
		Ok(())
	}


	fn drop_my_ass(&mut self, count: u32, ord_count: u32) {
		if count == 0 {
			// no need to waste a perfectly good sequence number
			return;
		}
		self.next_id = self.next_id.new_plus(count);
		println!("ORD CNT {} CNT {}", ord_count, count);
		if ord_count < count {
			self.wait_until = self.next_id.new_minus(ord_count);
		}
	}

	pub fn new_with_config(socket: U, config: EndpointConfig) -> Endpoint<U> {
		Endpoint {
			buf_min_space: config.max_msg_size + Header::BYTES,
			socket,
			buf: iter::repeat(0)
				.take(config.max_msg_size + config.buffer_grow_space + Header::BYTES)
				.collect(),
			buf_free_start: 0,
			next_id: ModOrd::ZERO,
			wait_until: ModOrd::ZERO,
			n: ModOrd::ZERO,
			largest_set_id_yielded: ModOrd::ZERO,
			inbox: HashMap::new(),
			inbox2: HashMap::new(),
			seen_before: HashSet::new(),
			inbox2_to_remove: None,
			max_yielded: ModOrd::BEFORE_ZERO,
			outbox: HashMap::new(),
			outbox2: HashMap::new(),
			peer_acked: ModOrd::BEFORE_ZERO,
			window_size: config.window_size,
		}
	}

	pub fn new(socket: U) -> Endpoint<U> {
		Self::new_with_config(socket, EndpointConfig::default())
	}

	pub fn send(&mut self, guarantee: Guarantee, payload: &[u8]) -> io::Result<usize> {
		self.get_set().send(guarantee, payload)
	}

	fn pre_yield(&mut self, set_id: ModOrd, id: ModOrd, del: bool) {
		if set_id > self.largest_set_id_yielded {
			self.largest_set_id_yielded = set_id;
			self.seen_before.clear();
			println!("Clearing `seen before`");
		}
		println!("seen before set is {:?}", &self.seen_before);
		self.seen_before.insert(id);
		if self.n < set_id {
			self.n = set_id;
			println!("n set to set_id={:?}", self.n);
		}
		if del {
			self.n = self.n.new_plus(1);
			println!("incrementing n because del. now is {:?}", self.n);
		}
		if self.max_yielded < id {
			self.max_yielded = id;
		}
	}

	fn too_stale_func(seq_difference: u32, age: time::Duration) -> bool {
		if age.as_secs() > 0 {
			true
		} else {
			let x = age.subsec_millis() + seq_difference * 8;
			println!("staleness {}", x);
			x > 1000 
		}
	}

	fn ready_from_inbox(&self) -> Option<ModOrd> {
		for (&id, msg) in self.inbox.iter() {
			println!("inbox1:: visiting id {:?}", id);
			if msg.h.wait_until <= self.n {
				return Some(id);
			}
		}
		None
	}

	fn ready_from_inbox2(&self) -> Option<ModOrd> {
		for (&id, msg) in self.inbox2.iter() {
			println!("inbox2:: visiting id2 {:?}", id);
			if msg.h.wait_until <= self.n {
				return Some(id);
			}
		}
		None
	}

	fn vacate_buffer(&mut self) {
		println!("VACATING INBOX 1 --> INBOX 2...");
		let mut count = 0;
		for (id, msg) in self.inbox.drain() {
			let payload = unsafe{&*msg.payload}.to_vec();
			println!("- VACATING MESSAGE WITH ID {:?} ({} bytes)...", id, payload.len());
			let h = msg.h;
			let owned_msg = OwnedMessage {
				h, payload,
			};
			self.inbox2.insert(id, owned_msg);
			count += 1;
		}
		assert!(self.inbox.is_empty());
		println!("VACATING COMPLETE. MOVED {} INBOX messages", count);

		println!("VACATING OUTBOX 1 --> OUTBOX 2...");
		let mut count = 0;
		for (id, (instant, bytes)) in self.outbox.drain() {
			let vec = unsafe{&*bytes}.to_vec();
			println!("- VACATING MESSAGE WITH ID {:?} ({} bytes)...", id, vec.len());
			self.outbox2.insert(id, (instant, vec));
			count += 1;
		}
		assert!(self.inbox.is_empty());
		println!("VACATING COMPLETE. MOVED {} OUTBOX messages", count);
		self.buf_free_start = 0;
	}

	pub fn known_duplicate(&mut self, header: &Header) -> bool {
		let id = header.id;
		println!("known?? {} {} {}", self.seen_before.contains(&id), self.inbox.contains_key(&id), self.inbox2.contains_key(&id));
	  	self.seen_before.contains(&id)
		|| self.inbox.contains_key(&id)
		|| self.inbox2.contains_key(&id) 
	}

	pub fn buf_cant_take_another(&self) -> bool {
		self.buf.len() - self.buf_free_start < self.buf_min_space
	}

	pub fn recv(&mut self) -> io::Result<&[u8]> {
		println!("largest_set_id_yielded {:?}", self.largest_set_id_yielded);
		{
			let intersect: HashSet<ModOrd> = self.inbox.keys().cloned().collect::<HashSet<_>>();
			let wang = self.inbox2.keys().cloned().collect::<HashSet<_>>();
			let i2 = intersect.intersection(
				& wang
			);
			println!("intersection {:?}", &i2);
			assert!(i2.count() == 0);

		}

		// first try in-line inbox
		if let Some(id) = self.ready_from_inbox() {
			let msg = self.inbox.remove(&id).unwrap();
			println!("getting id {:?} from inbox1 with {:?}", id, &msg.h);
			if self.inbox.is_empty() && self.outbox.is_empty() {
				println!("VACATING INTENTIONALLY (trivial)");
				self.vacate_buffer();
			}
			self.pre_yield(msg.h.set_id, msg.h.id, msg.h.del);
			println!("yeilding from store 1...");
			return Ok(unsafe{&*msg.payload});
		}

		// remove from inbox2 as possible
		if let Some(id) = self.inbox2_to_remove {
			println!("getting id {:?} from inbox2", id);
			println!("removing from inbox2 {:?}", id);
			self.inbox2.remove(&id);
			self.inbox2_to_remove = None;
		} 

		// next try inbox2 (growing owned storage)
		if let Some(id) = self.ready_from_inbox2() {
			let (set_id, id, del) = {
				let msg = self.inbox2.get(&id).unwrap();
				println!("getting id {:?} from inbox2 with {:?}", id, &msg.h);
				(msg.h.set_id, msg.h.id, msg.h.del)
			};
			self.pre_yield(set_id, id, del);
			self.inbox2_to_remove = Some(id); // will remove later
			println!("yeilding from store 2...");
			return Ok(&self.inbox2.get(&id).unwrap().payload);
		}

		// nothing ready from the inbox. receive messages until we can yield
		loop {
			println!("largest_set_id_yielded {:?}", self.largest_set_id_yielded);
			println!("SPACE IS {}", self.buf.len() - self.buf_free_start);
			if self.buf_cant_take_another() {
			// move messages from inbox1 to inbox2 to make space for a recv
				println!("HAVE TO VACATE");
				self.vacate_buffer();
			}

			println!("recv loop..");
			match self.socket.recv(&mut self.buf[self.buf_free_start..]) {
				Ok(bytes) => {
					println!("udp datagram with {} bytes ({} of which are payload)", bytes, bytes-Header::BYTES);
					assert!(bytes >= Header::BYTES);
					let h_starts_at = self.buf_free_start + bytes - Header::BYTES;
					let h = Header::read_from(& self.buf[h_starts_at..])?;
					if self.peer_acked < h.ack {
						self.peer_acked = h.ack;
						println!("peer ack is now {:?}", self.peer_acked);
					}

					if self.largest_set_id_yielded.abs_difference(h.set_id) > self.window_size {
						println!("OUTSIDE OF WINDOW");
						continue;
					}

					if self.known_duplicate(&h) {
						println!("DROPPING KNOWN DUPLICATE {:?}", h);
						continue;
					}
					let msg = Message {
						h,
						payload: (&self.buf[self.buf_free_start..h_starts_at]) as *const [u8],
					};
					self.buf_free_start = h_starts_at; // move the buffer right
					println!("buf starts at {} now...", self.buf_free_start);

					println!("\n::: channel sent {:?}", &msg);
					if msg.h.id.special() {
						println!("NO SEQ NUM");
						return Ok(unsafe{&*msg.payload})
					} else if msg.h.set_id < self.largest_set_id_yielded {
						println!("TOO OLD");
					} else if msg.h.wait_until > self.n {
						println!("NOT YET");
						if !self.inbox.contains_key(&msg.h.id) {
							println!("STORING");
							self.inbox.insert(msg.h.id, msg);
						}
					} else if self.seen_before.contains(&msg.h.id) {
						println!("seen before!");
					} else {
						self.pre_yield(msg.h.set_id, msg.h.id, msg.h.del);
						println!("yeilding in-place...");
						return Ok(unsafe{&*msg.payload})
					}
				},
				Err(e) => {
					return Err(e);
				}
			}
		}	
	}

	pub fn as_set<F,R>(&mut self, work: F) -> R
	where
		F: Sized + FnOnce(SetSender<U>) -> R,
		R: Sized,
	{
		work(self.get_set())
	}

	pub fn get_set(&mut self) -> SetSender<U> {
		let set_id = self.next_id;
		SetSender::new(self, set_id)
	}
}

////////////////////

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Guarantee {
	None,
	Order,
	Delivery,
}

//////////////////////////

#[derive(Debug)]
struct SetSender<'a, U: UdpLike + 'a>{
	endpoint: &'a mut Endpoint<U>,
	set_id: ModOrd,
	count: u32,
	ord_count: u32,
}

impl<'a, U> SetSender<'a, U> where U: UdpLike + 'a {

	fn new(endpoint: &mut Endpoint<U>, set_id: ModOrd) -> SetSender<U> {
		SetSender {

			endpoint,
			set_id,
			count: 0,
			ord_count: 0,
		}
	}

	fn send(&mut self, guarantee: Guarantee, payload: &[u8]) -> io::Result<usize> {
		if self.endpoint.buf_cant_take_another() {
			println!("VACATING BUFFER for SEND");
			self.endpoint.vacate_buffer();
		}
		let id = if guarantee == Guarantee::None {
			ModOrd::SPECIAL
		} else {
			self.set_id.new_plus(self.count)
		};
		let header = Header {
			ack: self.endpoint.max_yielded,
			set_id: self.set_id,
			id,
			wait_until: self.endpoint.wait_until,
			del: guarantee == Guarantee::Delivery,
		};

		let bytes_sent: usize = {
			let mut w = &mut self.endpoint.buf[self.endpoint.buf_free_start..];
			let len = w.write(payload)?;
			header.write_to(w)?;
			len + Header::BYTES
		};
		let new_end = self.endpoint.buf_free_start + bytes_sent;
		let msg_slice = & self.endpoint.buf[self.endpoint.buf_free_start..new_end];
		self.endpoint.socket.send(msg_slice)?;

		if guarantee == Guarantee::Delivery {
			// save into outbox and bump the buffer up
			self.endpoint.outbox.insert(id, (Instant::now(), msg_slice as *const [u8]));
			self.endpoint.buf_free_start = new_end;
		}

		println!("sending with g:{:?}. header is {:?}", guarantee, &header);
		if guarantee != Guarantee::None {
			self.count += 1;
			if guarantee != Guarantee::Delivery {
				self.ord_count += 1;
			}
		}
		Ok(bytes_sent)
	}
}

impl<'a, U> Drop for SetSender<'a, U> where U: UdpLike {
    fn drop(&mut self) {
        println!("Dropping!");
        self.endpoint.drop_my_ass(self.count, self.ord_count)
    }
}

/////////////////////////////////

#[derive(Debug, Clone)]
struct Header {
	id: ModOrd,
	set_id: ModOrd,
	ack: ModOrd,
	wait_until: ModOrd,
	del: bool,
}
impl Header {
	const BYTES: usize = 4*4 + 1;

	fn write_to<W: io::Write>(&self, mut w: W) -> io::Result<()> {
		self.ack.write_to(&mut w)?;
		self.id.write_to(&mut w)?;
		self.set_id.write_to(&mut w)?;
		self.wait_until.write_to(&mut w)?;
		w.write_u8(if self.del {0x01} else {0x00})?;
		Ok(())
	}

	fn read_from<R: io::Read>(mut r: R) -> io::Result<Self> {
		Ok(Header {
			ack: ModOrd::read_from(&mut r)?,
			id: ModOrd::read_from(&mut r)?,
			set_id: ModOrd::read_from(&mut r)?,
			wait_until: ModOrd::read_from(&mut r)?,
			del: r.read_u8()? == 0x01,
		})
	}
}
impl cmp::PartialEq for Header {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

#[derive(Clone)]
struct Message {
	h: Header,
	payload: *const [u8],
}
impl fmt::Debug for Message {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "Inbox1Msg {:?} payload ~= {:?}",
			self.h,
			String::from_utf8_lossy(unsafe{&*self.payload}),
		)
	}
}
impl cmp::PartialEq for Message {
    fn eq(&self, other: &Self) -> bool {
        self.h == other.h
    }
}

#[derive(Debug, Clone)]
struct OwnedMessage {
	h: Header,
	payload: Vec<u8>,
}
impl cmp::PartialEq for OwnedMessage {
    fn eq(&self, other: &Self) -> bool {
        self.h == other.h
    }
}

#[derive(Debug)]
struct BadUdp {
	messages: Vec<Vec<u8>>,
}

impl BadUdp {
	fn new() -> Self {
		BadUdp {
			messages: vec![],
		}
	}

	fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
		let m = buf.to_vec();
		self.messages.push(m.clone());
		self.messages.push(m);
		Ok(buf.len())
	}

	fn recv(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
		if self.messages.is_empty() {
			Err(io::ErrorKind::WouldBlock.into())
		} else {
			let i = thread_rng().gen_range(0, self.messages.len());
			let m = self.messages.remove(i);
			buf.write(&m)
		}
	}
}


trait UdpLike: Sized {
	fn send(&mut self, buf: &[u8]) -> io::Result<usize>;
	fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

impl UdpLike for UdpSocket {
	fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
		UdpSocket::send(self, buf)
	}
	fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		UdpSocket::recv(self, buf)
	}
}

impl UdpLike for BadUdp {
	fn send(&mut self, buf: &[u8]) -> io::Result<usize> {
		BadUdp::send(self, buf)
	}
	fn recv(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		BadUdp::recv(self, buf)
	}
}

//////////////////////// TEST////////////////

/*
	This tests a fake connection of some endpoint with itself
	the BadUdp object will connect messages but duplicate and jumble
	them before sending (no loss).
*/
#[test]
fn bad_udp() {

	let socket = BadUdp::new();
	let mut config = EndpointConfig::default();
	config.max_msg_size = 16;
	config.buffer_grow_space = 64;
	config.window_size = 32;

	println!("YAY");
	let mut e = Endpoint::new_with_config(socket, config);

	e.send(Guarantee::Delivery, b"Dank").unwrap();
	while let Ok(msg) = e.recv() {}

	e.send(Guarantee::Delivery, b"Lower...").unwrap();
	e.send(Guarantee::Delivery, b"...case").unwrap();

	e.as_set(|mut s| {
		for letter in ('a' as u8)..=('e' as u8) {
			s.send(Guarantee::Delivery, &vec![letter]).unwrap();
		}
	});

	e.send(Guarantee::Delivery, b"Numbers").unwrap();

	e.as_set(|mut s| {
		for letter in ('1' as u8)..=('3' as u8) {
			s.send(Guarantee::Delivery, &vec![letter]).unwrap();
		}
	});

	e.send(Guarantee::Delivery, b"Up...").unwrap();
	e.send(Guarantee::Delivery, b"...percase").unwrap();


	e.as_set(|mut s| {
		for letter in ('X' as u8)..=('Z' as u8) {
			s.send(Guarantee::Delivery, &vec![letter]).unwrap();
		}
	});

	e.send(Guarantee::Delivery, b"Done").unwrap();

	let mut got = vec![];
	while let Ok(msg) = e.recv() {
		let out: String = String::from_utf8_lossy(&msg[..]).to_string();
		println!("--> yielded: {:?}\n", &out);
		got.push(out);
	}
	println!("got: {:?}", got);
	e.send(Guarantee::Delivery, b"wahey").unwrap();
	while let Ok(msg) = e.recv() {}

	e.resend_lost().unwrap();

	println!("E {:#?}", e);
}

/*
	This test will check how well Rudp plays with MIO
	the idea is to set up a proper communication channel between two
	endpoints
*/
#[test]
fn mio_pair() {
	let poll = Poll::new().unwrap();
	let mut events = Events::with_capacity(128);
	let addrs = ["127.0.0.1:8888".parse().unwrap(), "127.0.0.1:8889".parse().unwrap()];
	let endpoints = {
		let f = |me_id, peer_id| {
			let sock = UdpSocket::bind(&addrs[me_id]).unwrap();
			sock.connect(addrs[peer_id]).unwrap();
			poll.register(&sock, Token(me_id), Ready::readable(), PollOpt::edge()).unwrap();
			Endpoint::new(sock)
		};
		[f(0, 1), f(1, 0)]
	};
}
