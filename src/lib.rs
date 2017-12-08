extern crate byteorder;
extern crate sodiumoxide;

use std::io::{Cursor,Error,Read,Write};
use std::sync::{Arc,Mutex};
use std::sync::mpsc::{Receiver,Sender,channel};
use std::thread;

use self::byteorder::{BigEndian,ReadBytesExt,WriteBytesExt};
use self::sodiumoxide::crypto::box_;
use self::sodiumoxide::crypto::secretbox;

const DEFAULT_CHUNK_SIZE : u32 = 16384;
const DEFAULT_POOL_SIZE : usize = 8;

#[derive(Debug,PartialEq)]
pub struct Header {
	encrypted_size: u32, 
	frame_size: u32,
	key: secretbox::Key,
	mime: String,
	nonce: secretbox::Nonce,
	num_frames: u32,
}

impl Header {

	pub fn decode<R: Read>(r: &mut R) -> Result<Header,Error> {
		let encrypted_size = r.read_u32::<BigEndian>()?;
		let frame_size = r.read_u32::<BigEndian>()?;
		let mut key = [0u8; secretbox::xsalsa20poly1305::KEYBYTES];
		r.read_exact(&mut key)?;	
		let key = secretbox::xsalsa20poly1305::Key::from_slice(&key).unwrap();
		let mime_size = r.read_u32::<BigEndian>()?;
		let mut mime = vec![0u8; mime_size as usize];
		r.read_exact(&mut mime)?;
		let mime = String::from_utf8(mime).unwrap();
		let mut nonce = [0u8; secretbox::xsalsa20poly1305::NONCEBYTES];
		r.read_exact(&mut nonce)?;
		let nonce = secretbox::xsalsa20poly1305::Nonce::from_slice(&nonce).unwrap();
		let num_frames = r.read_u32::<BigEndian>()?;
		Ok(Header {
			encrypted_size,
			frame_size,
			key,
			mime,
			nonce,
			num_frames,
		})
	}

	pub fn decrypt<R: Read>(pre_key: &box_::PrecomputedKey, r: &mut R) -> Result<Header,Error> {
		let mut nonce = [0u8; box_::curve25519xsalsa20poly1305::NONCEBYTES];
		r.read_exact(&mut nonce)?;
		let nonce = box_::curve25519xsalsa20poly1305::Nonce::from_slice(&nonce).unwrap();
		let size = r.read_u32::<BigEndian>()?;
		let mut encrypted = vec![0u8; size as usize];
		r.read_exact(&mut encrypted)?;
		let decrypted = box_::curve25519xsalsa20poly1305::open_precomputed(&encrypted, &nonce, pre_key).unwrap();
		Header::decode(&mut Cursor::new(decrypted))
	}

	pub fn encode<W: Write>(&self, w: &mut W) -> Result<(),Error> {
		w.write_u32::<BigEndian>(self.encrypted_size)?;
		w.write_u32::<BigEndian>(self.frame_size)?;
		w.write_all(&self.key.0)?;
		let mime = self.mime.as_bytes();
		w.write_u32::<BigEndian>(mime.len() as u32)?;
		w.write_all(mime)?;
		w.write_all(&self.nonce.0)?;
		w.write_u32::<BigEndian>(self.num_frames)
	}

	pub fn encrypt<W: Write>(&self, pre_key: &box_::PrecomputedKey, w: &mut W) -> Result<(),Error> {
		let mut encoded = Vec::with_capacity(self.encoded_size());
		self.encode(&mut encoded)?;
		let nonce = box_::curve25519xsalsa20poly1305::gen_nonce();
		w.write_all(&nonce.0)?;
		let encrypted = box_::curve25519xsalsa20poly1305::seal_precomputed(&encoded, &nonce, pre_key);
		w.write_u32::<BigEndian>(encrypted.len() as u32)?;
		w.write_all(&encrypted)
	}

	pub fn encoded_size(&self) -> usize {
		4 * 4 + secretbox::xsalsa20poly1305::KEYBYTES + secretbox::xsalsa20poly1305::NONCEBYTES + self.mime.as_bytes().len()
	}
}

#[derive(Debug)]
pub struct Job {
	bytes: Vec<u8>,
	idx: usize,
	key: secretbox::xsalsa20poly1305::Key,
	nonce: secretbox::xsalsa20poly1305::Nonce,
	op: u8,
}

impl Job {
	pub fn new(bytes: &[u8], idx: usize, key: &secretbox::xsalsa20poly1305::Key, nonce: &secretbox::xsalsa20poly1305::Nonce, op: u8) -> Job {
		Job {
			bytes: bytes.to_owned(),
			idx,
			key: key.clone(),
			nonce: nonce.clone(),
			op,
		}
	}
}

// ThreadPool adapted from https://doc.rust-lang.org/book/second-edition/ch20-04-storing-threads.html 
//
//	---------------- BEGIN LICENSE ----------------
//
// 	MIT License
//
// 	Copyright (c) 2011 The Rust Project Developers
//
// 	Permission is hereby granted, free of charge, to any person obtaining a copy
// 	of this software and associated documentation files (the "Software"), to deal
// 	in the Software without restriction, including without limitation the rights
// 	to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// 	copies of the Software, and to permit persons to whom the Software is
// 	furnished to do so, subject to the following conditions:
//
// 	The above copyright notice and this permission notice shall be included in all
// 	copies or substantial portions of the Software.
//
// 	THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// 	IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// 	FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// 	AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// 	LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// 	OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// 	SOFTWARE.

#[derive(Debug)]
pub struct Pool {
	// running: AtomicBool,
	rx: Receiver<(Vec<u8>,usize)>,
	tx: Sender<Job>,
	workers: Vec<Worker>
}

impl Pool {

	pub fn new(size: usize) -> Pool {
		// let running = ATOMIC_BOOL_INIT;
		let (tx1, rx1) = channel();
		let (tx2, rx2) = channel();
		let rx1 = Arc::new(Mutex::new(rx1));
		let mut workers = Vec::with_capacity(size);
		for id in 0..size {
			workers.push(Worker::new(id, rx1.clone(), tx2.clone()));
		}
		Pool{
			rx: rx2,
			tx: tx1,
			workers,
		}
	}

	pub fn recv(&self) -> Result<(Vec<u8>,usize),String> {
		self.rx.recv().map_err(|err| err.to_string())
	}

	pub fn send(&self, job: Job) -> Result<(),String> {
		self.tx.send(job).map_err(|err| err.to_string())
	}
}

#[derive(Debug)]
pub struct Worker {
	id: usize, 
	thread: thread::JoinHandle<()>,
}

impl Worker {
	pub fn new(id: usize, rx: Arc<Mutex<Receiver<Job>>>, tx: Sender<(Vec<u8>,usize)>) -> Worker {
		let thread = thread::spawn(move || {
			loop {
				if let Ok(lock) = rx.lock() {
					if let Ok(job) = lock.recv() {
						if job.op == DECRYPT {
							let decrypted = secretbox::open(&job.bytes, &job.nonce, &job.key).unwrap();
							tx.send((decrypted, job.idx)).unwrap();	
						} else if job.op == ENCRYPT {
							let encrypted = secretbox::seal(&job.bytes, &job.nonce, &job.key);
							tx.send((encrypted, job.idx)).unwrap();	
						}
					}
				}
			}
		});
		Worker{
			id,
			thread,
		}
	}
}

//	---------------- END LICENSE ----------------

const DECRYPT : u8 = 0x01;
const ENCRYPT : u8 = 0x02;

#[derive(Debug)]
pub struct Pipe<'a, T: Read + Write + 'a, U: Read + Write + 'a>{
	dest: &'a mut U,
	pool: Pool,
	pre_key: &'a box_::PrecomputedKey,
	source: &'a mut T,
}

macro_rules! vecvec {
    ($t:ty; $e:expr) => {{
        let mut vec: Vec<Vec<$t>> = Vec::with_capacity($e);
        for _ in 0..$e {
            vec.push(vec![]);
        } 
        vec
    }};
}

impl<'a, T: Read + Write + 'a, U: Read + Write + 'a> Read for Pipe<'a, T, U> {
	fn read(&mut self, buf: &mut [u8]) -> Result<usize,Error> {
		self.dest.read(buf)
	}
}

impl<'a, T: Read + Write + 'a, U: Read + Write + 'a> Write for Pipe<'a, T, U> {

	fn flush(&mut self) -> Result<(),Error> {
		self.source.flush()
	}

	fn write(&mut self, buf: &[u8]) -> Result<usize,Error> {
		self.source.write(buf)
	}
}

impl<'a, T: Read + Write + 'a, U: Read + Write + 'a> Pipe<'a, T, U> {

	pub fn default(pre_key: &'a box_::curve25519xsalsa20poly1305::PrecomputedKey, source: &'a mut T, dest: &'a mut U) -> Pipe<'a,T,U> {
		Pipe::new(pre_key, source, dest, DEFAULT_POOL_SIZE)
	}

	pub fn new(pre_key: &'a box_::curve25519xsalsa20poly1305::PrecomputedKey, source: &'a mut T, dest: &'a mut U, pool_size: usize) -> Pipe<'a,T,U> {
		let pool = Pool::new(pool_size);
		Pipe{
			dest, 
			pool,
			pre_key,
			source,
		}
	}

	pub fn header(&self, chunk_size: u32, file_size: u32, key: &secretbox::Key, mime: String, nonce: &secretbox::Nonce) -> Header {
		let overhead = secretbox::xsalsa20poly1305::MACBYTES as u32; 
		let encrypted_size;
		let frame_size = chunk_size + overhead;
		let mut num_frames = file_size / chunk_size;
		let rem = file_size % chunk_size;
		if rem == 0 {
			encrypted_size = num_frames * frame_size;
		} else {
			encrypted_size = num_frames * frame_size + rem + overhead;
			num_frames += 1;
		}
		Header{
			encrypted_size,
			frame_size,			
			key: key.clone(),
			mime,
			nonce: nonce.clone(),
			num_frames,
		}
	}

	pub fn decrypt(&mut self) -> Result<(),String> {
		let header = Header::decrypt(&self.pre_key, self.source).map_err(|err| err.to_string())?;
		let mut frame = vec![0u8; header.frame_size as usize];
		let mut idx = 0;
		let mut size_left = header.encrypted_size;
		while size_left > header.frame_size {
			self.source.read_exact(&mut frame).map_err(|err| err.to_string())?;
			self.pool.send(Job::new(&frame, idx, &header.key, &header.nonce, DECRYPT))?;
			idx += 1;
			size_left -= header.frame_size;
		}
		if size_left > 0 {
			frame.truncate(size_left as usize);
			self.source.read_exact(&mut frame).map_err(|err| err.to_string())?;
			self.pool.send(Job::new(&frame, idx, &header.key, &header.nonce, DECRYPT))?;
		}
		let mut chunks = vecvec![u8; header.num_frames as usize];
		let mut next = 0;
		while next < header.num_frames {
			if !chunks[next as usize].is_empty() {
				self.dest.write_all(&chunks[next as usize]).map_err(|err| err.to_string())?;
				next += 1;
			}
			let (decrypted, idx) = self.pool.recv()?;
			if idx == next as usize {
				self.dest.write_all(&decrypted).map_err(|err| err.to_string())?;
				next += 1;
			} else {
				chunks[idx as usize] = decrypted;
			}
		}
		Ok(())
	}

	pub fn encrypt(&mut self, chunk_size: u32, file_size: u32, mime: String) -> Result<(),String> {
		let mut chunk = vec![0u8; chunk_size as usize];
		let key = secretbox::gen_key();
		let nonce = secretbox::gen_nonce();
		let header = self.header(chunk_size, file_size, &key, mime, &nonce);
		let mut size_left = file_size;
		header.encrypt(&self.pre_key, &mut self.dest).map_err(|err| err.to_string())?;
		let mut idx = 0;
		while size_left > chunk_size {
			self.source.read_exact(&mut chunk).map_err(|err| err.to_string())?;
			self.pool.send(Job::new(&chunk, idx, &key, &nonce, ENCRYPT))?;
			idx += 1;
			size_left -= chunk_size;
		} 
		if size_left > 0 {
			chunk.truncate(size_left as usize);
			self.source.read_exact(&mut chunk).map_err(|err| err.to_string())?;
			self.pool.send(Job::new(&chunk, idx, &key, &nonce, ENCRYPT))?;
		}
		let mut frames = vecvec![u8; header.num_frames as usize];
		let mut next = 0;
		while next < header.num_frames {
			if !frames[next as usize].is_empty() {
				self.dest.write_all(&frames[next as usize]).map_err(|err| err.to_string())?;
				next += 1;
			}
			let (encrypted, idx) = self.pool.recv().map_err(|err| err.to_string())?;
			if idx == next as usize {
				self.dest.write_all(&encrypted).map_err(|err| err.to_string())?;
				next += 1;
			} else {
				frames[idx as usize] = encrypted; 
			}
		}
		Ok(())
	}
}

#[cfg(test)]
mod test {

	use super::*;
	use std::fs::File;

	fn box_keys() -> (box_::curve25519xsalsa20poly1305::PrecomputedKey, box_::curve25519xsalsa20poly1305::PublicKey, box_::curve25519xsalsa20poly1305::SecretKey) {
		let (mut pub_key, _) = box_::curve25519xsalsa20poly1305::gen_keypair();
		let (_, mut sec_key) = box_::curve25519xsalsa20poly1305::gen_keypair();
		let pre_key = box_::curve25519xsalsa20poly1305::precompute(&mut pub_key, &mut sec_key);
		(pre_key, pub_key, sec_key)
	}

	fn new_header(key: &secretbox::xsalsa20poly1305::Key, nonce: &secretbox::xsalsa20poly1305::Nonce) -> Header {
		let encrypted_size = 49225 + secretbox::xsalsa20poly1305::MACBYTES as u32;
		let frame_size = DEFAULT_CHUNK_SIZE + secretbox::xsalsa20poly1305::MACBYTES as u32;
		Header{
			encrypted_size,
			frame_size,
			key: key.clone(),
			mime: "image/png".to_owned(),
			nonce: nonce.clone(),
			num_frames: 4,
		}
	}

	#[test]
	fn test_header() {
		let key = secretbox::xsalsa20poly1305::gen_key();
		let nonce = secretbox::xsalsa20poly1305::gen_nonce();
		let header = new_header(&key, &nonce);

		let mut rw = Cursor::new(Vec::new());
		header.encode(&mut rw).unwrap();
		rw.set_position(0);
		assert_eq!(header, Header::decode(&mut rw).unwrap());

		let mut rw = Cursor::new(Vec::new());
		let (pre_key, _, _) = box_keys();
		header.encrypt(&pre_key, &mut rw).unwrap();
		rw.set_position(0);
		assert_eq!(header, Header::decrypt(&pre_key, &mut rw).unwrap())
	}

	#[test]
	fn test_pipe() {
		let (pre_key, _, _) = box_keys();
		let mut source = File::open("./test.mp3").unwrap();
		let meta = source.metadata().unwrap();
		let file_size = meta.len() as u32;
		let mut dest1 = Cursor::new(Vec::new());
		let mut dest2 = Cursor::new(Vec::new());

		{
			let mut pipe1 = Pipe::default(&pre_key, &mut source, &mut dest1);
			pipe1.encrypt(DEFAULT_CHUNK_SIZE, file_size, "audio/mp3".to_owned()).unwrap();
			pipe1.dest.set_position(0);

			let mut pipe2 = Pipe::default(&pre_key, &mut pipe1, &mut dest2);
			pipe2.decrypt().unwrap();
			pipe2.dest.set_position(0);
		}	

		let mut file = File::open("./test.mp3").unwrap();
		let mut plain_bytes = vec![0u8; file_size as usize];
		file.read_exact(&mut plain_bytes).unwrap();

		assert_eq!(plain_bytes, dest2.into_inner());
	}
}