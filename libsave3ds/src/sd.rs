use crate::aes_ctr_file::AesCtrFile;
use crate::disk_file::DiskFile;
use crate::error::*;
use crate::key_engine::*;
use sha2::*;
use std::path::*;
use std::rc::Rc;

pub struct Sd {
    path: PathBuf,
    key: [u8; 16],
}

impl Sd {
    pub fn new(sd_path: &str, key_x: [u8; 16], key_y: [u8; 16]) -> Result<Sd, Error> {
        let path = std::fs::read_dir(
            PathBuf::from(sd_path)
                .join("Nintendo 3DS")
                .join(crate::hash_movable(key_y)),
        )?
        .next()
        .ok_or(Error::NoSd)??
        .path();
        let key = scramble(key_x, key_y);
        Ok(Sd { path, key })
    }

    pub fn open(&self, path: &[&str]) -> Result<AesCtrFile, Error> {
        let file_path = path.iter().fold(self.path.clone(), |a, b| a.join(b));
        let file = Rc::new(DiskFile::new(
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(file_path)?,
        )?);

        let hash_path: Vec<u8> = path
            .iter()
            .map(|s| std::iter::once(b'/').chain(s.bytes()))
            .flatten()
            .chain(std::iter::once(0))
            .map(|c| std::iter::once(c).chain(std::iter::once(0)))
            .flatten()
            .collect();

        let mut hasher = Sha256::new();
        hasher.input(&hash_path);
        let hash = hasher.result();
        let mut ctr = [0; 16];
        for (i, c) in ctr.iter_mut().enumerate() {
            *c = hash[i] ^ hash[i + 16];
        }

        Ok(AesCtrFile::new(file, self.key, ctr))
    }
}