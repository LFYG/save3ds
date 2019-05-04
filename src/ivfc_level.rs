use crate::random_access_file::*;
use sha2::*;
use std::cell::RefCell;
use std::rc::Rc;

const BLOCK_UNVERIFIED: u8 = 0;
const BLOCK_VERIFIED: u8 = 1;
const BLOCK_MODIFIED: u8 = 2;

pub struct IvfcLevel {
    hash: Rc<RandomAccessFile>,
    data: Rc<RandomAccessFile>,
    block_len: usize,
    len: usize,
    status: RefCell<Vec<u8>>,
}

impl IvfcLevel {
    pub fn new(
        hash: Rc<RandomAccessFile>,
        data: Rc<RandomAccessFile>,
        block_len: usize,
    ) -> IvfcLevel {
        let len = data.len();
        let block_count = 1 + (len - 1) / block_len;
        assert_eq!(block_count * 0x20, hash.len());
        let chunk_count = 1 + (block_count - 1) / 4;
        IvfcLevel {
            hash,
            data,
            block_len,
            len,
            status: RefCell::new(vec![0; chunk_count]),
        }
    }

    pub fn get_status(&self, block_index: usize) -> u8 {
        (self.status.borrow()[block_index / 4] >> ((block_index % 4) * 2)) & 3
    }

    pub fn set_status(&self, block_index: usize, status: u8) {
        let mut status_list = self.status.borrow_mut();
        let i = block_index / 4;
        let j = (block_index % 4) * 2;
        status_list[i] &= !(3 << j);
        status_list[i] |= status << j;
    }
}

impl RandomAccessFile for IvfcLevel {
    fn read(&self, pos: usize, buf: &mut [u8]) -> Result<(), Error> {
        let end = pos + buf.len();
        assert!(end <= self.len());

        // block index range the operation covers
        let begin_block = pos / self.block_len;
        let end_block = 1 + (end - 1) / self.block_len;

        for i in begin_block..end_block {
            // data range of this block
            let data_begin_as_block = i * self.block_len;
            let data_end_as_block = std::cmp::min((i + 1) * self.block_len, self.len);

            let mut block_buf = vec![0; self.block_len];
            self.data.read(
                data_begin_as_block,
                &mut block_buf[0..data_end_as_block - data_begin_as_block],
            )?;
            if self.get_status(i) == BLOCK_UNVERIFIED {
                let mut hasher = Sha256::new();
                hasher.input(&block_buf);
                let hash = hasher.result();
                let mut hash_stored = [0; 0x20];
                self.hash.read(i * 0x20, &mut hash_stored)?;
                if hash[..] != hash_stored[..] {
                    return Err(Error::HashMismatch);
                }
                self.set_status(i, BLOCK_VERIFIED);
            }

            // data range to read within this block
            let data_begin = std::cmp::max(data_begin_as_block, pos);
            let data_end = std::cmp::min(data_end_as_block, end);

            buf[data_begin - pos..data_end - pos].copy_from_slice(
                &block_buf[data_begin - data_begin_as_block..data_end - data_begin_as_block],
            );
        }

        Ok(())
    }
    fn write(&self, pos: usize, buf: &[u8]) -> Result<(), Error> {
        let end = pos + buf.len();
        assert!(end <= self.len());
        self.data.write(pos, buf)?;

        // block index range the operation covers
        let begin_block = pos / self.block_len;
        let end_block = 1 + (end - 1) / self.block_len;

        for i in begin_block..end_block {
            self.set_status(i, BLOCK_MODIFIED);
        }

        Ok(())
    }
    fn len(&self) -> usize {
        self.len
    }
    fn commit(&self) -> Result<(), Error> {
        // Recalculate the hash for modified blocks
        let block_count = 1 + (self.len - 1) / self.block_len;
        for i in 0..block_count {
            if self.get_status(i) == BLOCK_MODIFIED {
                let mut buf = vec![0; self.block_len];
                let begin = i * self.block_len;
                let end = std::cmp::min((i + 1) * self.block_len, self.len);
                self.data.read(begin, &mut buf[0..end - begin])?;
                let mut hasher = Sha256::new();
                hasher.input(buf);
                let hash = hasher.result();
                self.hash.write(i * 0x20, &hash)?;
                self.set_status(i, BLOCK_VERIFIED);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::ivfc_level::IvfcLevel;
    use crate::memory_file::MemoryFile;
    use crate::random_access_file::*;
    use std::rc::Rc;

    #[test]
    fn fuzz() {
        use rand::distributions::Standard;
        use rand::prelude::*;

        let mut rng = rand::thread_rng();
        for _ in 0..10 {
            let len = rng.gen_range(1, 10_000);
            let block_len = rng.gen_range(1, 100);
            let block_count = 1 + (len - 1) / block_len;
            let hash_len = block_count * 0x20;
            let hash = Rc::new(MemoryFile::new(
                rng.sample_iter(&Standard).take(hash_len).collect(),
            ));
            let data = Rc::new(MemoryFile::new(
                rng.sample_iter(&Standard).take(len).collect(),
            ));
            let mut ivfc_level = IvfcLevel::new(hash.clone(), data.clone(), block_len);
            let mut buf = vec![0; len];
            match ivfc_level.read(0, &mut buf) {
                Err(Error::HashMismatch) => (),
                _ => unreachable!(),
            }
            let init: Vec<u8> = rng.sample_iter(&Standard).take(len).collect();
            ivfc_level.write(0, &init).unwrap();
            let plain = MemoryFile::new(init);

            for _ in 0..100 {
                let operation = rng.gen_range(1, 10);
                if operation == 1 {
                    ivfc_level.commit().unwrap();
                    ivfc_level = IvfcLevel::new(hash.clone(), data.clone(), block_len);
                } else if operation < 4 {
                    ivfc_level.commit().unwrap();
                } else {
                    let pos = rng.gen_range(0, len);
                    let data_len = rng.gen_range(1, len - pos + 1);
                    if operation < 7 {
                        let mut a = vec![0; data_len];
                        let mut b = vec![0; data_len];
                        ivfc_level.read(pos, &mut a).unwrap();
                        plain.read(pos, &mut b).unwrap();
                        assert_eq!(a, b);
                    } else {
                        let a: Vec<u8> = rng.sample_iter(&Standard).take(data_len).collect();
                        ivfc_level.write(pos, &a).unwrap();
                        plain.write(pos, &a).unwrap();
                    }
                }
            }
        }
    }
}