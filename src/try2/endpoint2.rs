#![allow(dead_code)] //////////// REMOVE REMOVE DEBUG DEBUG TODO TODO

use helper::*;
use std::{
	collections::{
		HashMap,
		HashSet,
	},
	io, fmt, iter, cmp,
	time::Instant,
	io::ErrorKind,
};
use byteorder::{ReadBytesExt, WriteBytesExt};
use mod_ord::ModOrd;

trait VeryUdpLike {
	fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddr)>;
	fn send_to(&self, buf: &[u8], addr: &SocketAddr) -> Result<usize>;
}


#[derive(Debug)]
pub struct Endpoint<U: VeryUdpLike> {

	//fundamentals
	config: EndpointConfig,
	socket: U,

	// buffer
	buf: Vec<u8>,
	buf_free_start: usize,
	out_buf_written: usize,
	buf_min_space: usize,

	// state 
	states: HashMap<SocketAddr, SessionState>,
}




impl<U> Endpoint<U> where U: VeryUdpLike {
///// PUBLIC

	/// Discard acknowledged outputs, resend lost outputs and send a heartbeat if
	/// necessary.
	pub fn maintain(&mut self) -> io::Result<()> {
		for s in states.iter_mut() { s.maintain() }
	}

	
	/// Create a new Endpoint around the given Udp-like object, with the given
	/// configuration.
	pub fn new_with_config(socket: U, config: EndpointConfig) -> Endpoint<U> {
		let buf_min_space = config.max_msg_size + Header::BYTES;
		let buflen = config.max_msg_size + config.buffer_grow_space + Header::BYTES;
		Endpoint {
			config, socket, buf_min_space,
			buf_free_start: 0,
			out_buf_written: 0,
			buf: iter::repeat(0)
				.take(buflen)
				.collect(),
			inbox2_to_remove: None,
			states: HashMap::new(),
		}
	}

	
	/// Create a new Endpoint around the given Udp-like object, with the default
	/// configuration.
	pub fn new(socket: U) -> Endpoint<U> {
		Self::new_with_config(socket, EndpointConfig::default())
	}

	
	/// Attempt to yield a message from the peer Endpoint that is ready for receipt.
	/// May block only if the wrapped Udp-like object may block.
	/// recv() calls may not call the inner receive, depending on the contents
	/// of the inbox.
	///
	/// Fatal errors return `Err(_)`
	/// Reads that fail because they would block return `Ok(None)`
	/// Successful reads return `Ok(Some(x))`, where x is an in-place slice into the internal
	/// buffer; thus, you need to drop the slice before interacting with the 
	/// Endpoint again. 
	pub fn recv_from(&mut self) -> io::Result<Option<(&SocketAddr, &mut [u8])>> {

		for (peer_addr, state) in self.states.iter_mut() {
			//TODO maybe ack here
			if let Some(msg) = state.pop_inbox_ready() {
				return Ok(Some((recv_from, msg)))
			}
		}

		// nothing ready from the inbox. receive messages until we can yield
		loop {
			if self.buf_cant_take_another() {
				self.vacate_buffer();
			}

			match self.socket.recv(&mut self.buf[self.buf_free_start..]) {
				Ok(0) => {
					let _ = self.maybe_ack();
					return Ok(None)
				},
				Err(e) => {
					let _ = self.maybe_ack();
					return if e.kind() == ErrorKind::WouldBlock {
						return Ok(None)
					} else {
						Err(ErrorKind::WouldBlock.into())
					};
				},
				Ok(ModOrd::BYTES) => {
					let ack = ModOrd::read_from(& self.buf[self.buf_free_start..(self.buf_free_start+ModOrd::BYTES)]).unwrap();
					self.digest_incoming_ack(ack);
				},
				Ok(bytes) if bytes >= Header::BYTES => {
					let h_starts_at = self.buf_free_start + bytes - Header::BYTES;
					let h = Header::read_from(& self.buf[h_starts_at..])?;
					self.digest_incoming_ack(h.ack);
					if self.invalid_header(&h) || self.known_duplicate(&h) {
						continue;
					}
					let msg = Message {
						h,
						payload: (&mut self.buf[self.buf_free_start..h_starts_at]) as *mut [u8],
					};

					// BIG IF ELSE BRANCH.
					if msg.h.id.special() {
						/* read a 'None' guarantee message.
						shift - YES
						store - NO
						yield = YES
						*/
						self.maybe_ack()?;
						return Ok(Some(unsafe{&mut *msg.payload}))
					} else if msg.h.set_id < self.largest_set_id_yielded {
						/* previous-set message. Its OLD data. Must discard to be safe.
						shift - NO
						store - NO
						yield = NO
						*/
						continue;
					} else if msg.h.wait_until > self.n {
						/* future message.
						shift - YES
						store - YES
						yield = NO
						*/
						if !self.inbox.contains_key(&msg.h.id) {
							self.inbox.insert(msg.h.id, msg);
						}
						// shift the buffer right. don't want to obliterate the data
						self.buf_free_start = h_starts_at; 
					} else if self.seen_before.contains(&msg.h.id) {
						/* current-set message already yielded
						shift - NO
						store - NO
						yield = NO
						*/
					} else {
						/* ORDER or DELIVERY message, but we can yield it right away
						shift - NO
						store - NO
						yield = YES
						*/
						self.pre_yield(msg.h.set_id, msg.h.id, msg.h.del);
						self.maybe_ack()?;
						return Ok(Some(unsafe{&mut *msg.payload}))
					}					
				},
				Ok(_) => (), // invalid size datagram. improper header or bogus.
			}
		}	
	}

	
	/// Convenience function that passes a new `SetSender` into the given closure.
	/// See `new_set` for more information.
	pub fn as_set<F,R>(&mut self, work: F) -> R
	where
		F: Sized + FnOnce(SetSender<U>) -> R,
		R: Sized,
	{
		work(self.new_set())
	}

	
	/// The `Endpoint` itself implements `Sender`, allowing it to send messages.
	/// `new_set` returns a `SetSender` object, which implements the same trait.
	/// All messages sent by this setsender object have the added semantics of
	/// relaxed ordering _between_ them. 
	pub fn new_set(&mut self) -> SetSender<U> {
		if self.out_buf_written > 0 {
			match self.config.new_set_unsent_action {
				NewSetUnsent::Panic => panic!(
					"Endpoint created new set \
					with non-empty write buffer! \
					(Configuration requested a panic)."
				),
				NewSetUnsent::Clear => self.out_buf_written = 0,
				NewSetUnsent::IntoSet => (), // keep the bytes
			}
		}
		self.inner_new_set()
	}



////////////// PRIVATE

	#[inline]
	fn header_in_range(&self, h: &Header) -> bool {
		if self.largest_set_id_yielded.abs_difference(h.set_id)
		> self.config.window_size {
			// outside of window
			false
		}
	}

	#[inline(always)]
	fn inner_new_set(&mut self) -> SetSender<U> {
		let set_id = self.next_id;
		SetSender::new(self, set_id)
	}

	fn pre_yield(&mut self, set_id: ModOrd, id: ModOrd, del: bool) {
		if set_id > self.largest_set_id_yielded {
			self.largest_set_id_yielded = set_id;
			self.seen_before.clear();
		}
		self.seen_before.insert(id);
		if self.n < set_id {
			self.n = set_id;
		}
		if del {
			self.n = self.n.new_plus(1);
		}
		if self.max_yielded < id {
			self.max_yielded = id;
		}
	}

	/*
	Empty the big buffer. Need to make sure that any inbox/outbox data
	that is still inside is relocated to the secondary storage.
	This requires copying over.
	*/
	fn vacate_buffer(&mut self) {
		for state in self.states.values_mut() {
			state.vacate_primary()
		}
		self.buf_free_start = 0;
	}

	fn known_duplicate(&mut self, header: &Header) -> bool {
		let id = header.id;
	  	self.seen_before.contains(&id)
		|| self.inbox.contains_key(&id)
		|| self.inbox2.contains_key(&id) 
	}

	fn buf_cant_take_another(&self) -> bool {
		self.buf.len() - self.buf_free_start < self.buf_min_space
	}

	fn digest_incoming_ack(&mut self, ack: ModOrd) {
		if self.peer_acked < ack {
			self.peer_acked = ack;
		}
	}
}



impl<U> Sender for Endpoint<U> where U: VeryUdpLike {
	fn send_written(&mut self, guarantee: Guarantee) -> io::Result<usize> {
		self.inner_new_set().send_written(guarantee)
	}

	fn clear_written(&mut self) {
		self.out_buf_written = 0;
	}
}

impl<U> io::Write for Endpoint<U> where U: VeryUdpLike {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    	let b = (&mut self.buf[(self.buf_free_start + self.out_buf_written)..]).write(bytes)?;
    	self.out_buf_written += b;
    	Ok(b)
    }

    fn flush(&mut self) -> io::Result<()> {
    	Ok(())
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

	fn is_valid(&self) -> bool {
		if h.id < h.set_id {
			// set id cannot be AFTER the id
			false
		} else if h.wait_until > h.id {
			// cannot wait for a message after self
			false
		} else {
			true
		}
	}

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


////////////////////////////////////////////////////////////////////////////////


/// An Endpoint can send payloads of data. However, all messages sent by a single
/// `SetSender` object of the endpoint are semantically grouped together into an 
/// unordered set. A new set cannot be defined until the current one is dropped.
/// 
/// Note that the concept of _sending_
#[derive(Debug)]
pub struct SetSender<'a, U: VeryUdpLike + 'a>{
	endpoint: &'a mut Endpoint<U>,
	set_id: ModOrd,
	count: u32,
	ord_count: u32,
}

impl<'a, U> SetSender<'a, U> where U: VeryUdpLike + 'a {
	fn new(endpoint: &mut Endpoint<U>, set_id: ModOrd) -> SetSender<U> {
		SetSender {
			endpoint,
			set_id,
			count: 0,
			ord_count: 0,
		}
	}
}

impl<'a, U> Sender for SetSender<'a, U> where U: VeryUdpLike + 'a {
	fn send_written(&mut self, guarantee: Guarantee) -> io::Result<usize> {
		if self.endpoint.buf_cant_take_another() {
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

		let payload_end = self.endpoint.buf_free_start+self.endpoint.out_buf_written;
		header.write_to(&mut self.endpoint.buf[payload_end..])?;
		let bytes_sent = self.endpoint.out_buf_written + Header::BYTES;
		self.endpoint.out_buf_written = 0;
		let new_end = self.endpoint.buf_free_start + bytes_sent;
		let msg_slice = &mut self.endpoint.buf[self.endpoint.buf_free_start..new_end];
		self.endpoint.socket.send(msg_slice)?;

		if guarantee == Guarantee::Delivery {
			// save into outbox and bump the buffer up
			self.endpoint.outbox.insert(id, (Instant::now(), msg_slice as *mut [u8]));
			self.endpoint.buf_free_start = new_end;
		}

		if guarantee != Guarantee::None {
			self.count += 1;
			if guarantee != Guarantee::Delivery {
				self.ord_count += 1;
			}
		}
		Ok(bytes_sent)
	}

	fn clear_written(&mut self) {
		self.endpoint.clear_written()
	}
}

impl<'a, U> Drop for SetSender<'a, U> where U: VeryUdpLike {
    fn drop(&mut self) {
		if self.count == 0 {
			// set was empty. nothing to do here
			return;
		}
		// increment the next_id by the number of IDs that the set contained
		self.endpoint.next_id = self.endpoint.next_id.new_plus(self.count);
		if self.ord_count < self.count {
			// there was at least ONE delivery message. future sets must wait fo
			// all of them (instead of waiting for whatever the previous set was waiting for)
			self.endpoint.wait_until = self.endpoint.next_id.new_minus(self.ord_count);
		}
    }
}

impl<'a, U> io::Write for SetSender<'a, U> where U: VeryUdpLike {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
    	self.endpoint.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
    	Ok(())
    }
}
