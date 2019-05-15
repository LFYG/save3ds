use crate::disa::Disa;
use crate::error::*;
use crate::fat::*;
use crate::fs_meta::{self, FileInfo};
use crate::memory_file::MemoryFile;
use crate::random_access_file::*;
use crate::save_ext_common::*;
use crate::signed_file::*;
use crate::sub_file::SubFile;
use byte_struct::*;
use std::rc::Rc;

#[derive(ByteStruct, Clone)]
#[byte_struct_le]
pub struct SaveFile {
    pub next: u32,
    pub padding1: u32,
    pub block: u32,
    pub size: u64,
    pub padding2: u32,
}

impl FileInfo for SaveFile {
    fn set_next(&mut self, index: u32) {
        self.next = index;
    }
    fn get_next(&self) -> u32 {
        self.next
    }
}

type FsMeta = fs_meta::FsMeta<SaveExtKey, SaveExtDir, SaveExtKey, SaveFile>;
type DirMeta = fs_meta::DirMeta<SaveExtKey, SaveExtDir, SaveExtKey, SaveFile>;
type FileMeta = fs_meta::FileMeta<SaveExtKey, SaveExtDir, SaveExtKey, SaveFile>;

pub struct NandSaveSigner {
    pub id: u32,
}

impl Signer for NandSaveSigner {
    fn block(&self, mut data: Vec<u8>) -> Vec<u8> {
        let mut result = Vec::from(&b"CTR-SYS0"[..]);
        result.extend(&self.id.to_le_bytes());
        result.extend(&[0; 4]);
        result.append(&mut data);
        result
    }
}

pub struct CtrSav0Signer {}

impl Signer for CtrSav0Signer {
    fn block(&self, mut data: Vec<u8>) -> Vec<u8> {
        let mut result = Vec::from(&b"CTR-SAV0"[..]);
        result.append(&mut data);
        result
    }
}

pub struct SdSaveSigner {
    pub id: u64,
}
impl Signer for SdSaveSigner {
    fn block(&self, data: Vec<u8>) -> Vec<u8> {
        let mut result = Vec::from(&b"CTR-SIGN"[..]);
        result.extend(&self.id.to_le_bytes());
        result.append(&mut CtrSav0Signer {}.hash(data));
        result
    }
}

#[derive(ByteStruct)]
#[byte_struct_le]
struct SaveHeader {
    magic: [u8; 4],
    version: u32,
    fs_info_offset: u64,
    image_size: u64,
    image_block_len: u32,
    padding: u32,
}

pub struct SaveData {
    disa: Rc<Disa>,
    fat: Rc<Fat>,
    fs: Rc<FsMeta>,
    block_len: usize,
}

pub enum SaveDataType {
    Nand([u8; 16], u32),
    Sd([u8; 16], u64),
    Bare,
}

impl SaveData {
    pub fn from_vec(v: Vec<u8>, save_data_type: SaveDataType) -> Result<Rc<SaveData>, Error> {
        let file = Rc::new(MemoryFile::new(v));
        SaveData::new(file, save_data_type)
    }

    pub fn new(
        file: Rc<RandomAccessFile>,
        save_data_type: SaveDataType,
    ) -> Result<Rc<SaveData>, Error> {
        let signer: Option<(Box<Signer>, [u8; 16])> = match save_data_type {
            SaveDataType::Bare => None,
            SaveDataType::Nand(key, id) => Some((Box::new(NandSaveSigner { id }), key)),
            SaveDataType::Sd(key, id) => Some((Box::new(SdSaveSigner { id }), key)),
        };

        let disa = Rc::new(Disa::new(file, signer)?);
        let header: SaveHeader = read_struct(disa[0].as_ref(), 0)?;
        if header.magic != *b"SAVE" || header.version != 0x40000 {
            return make_error(Error::MagicMismatch);
        }
        let fs_info: FsInfo = read_struct(disa[0].as_ref(), header.fs_info_offset as usize)?;
        if fs_info.data_block_count != fs_info.fat_size {
            return make_error(Error::SizeMismatch);
        }

        let dir_hash = Rc::new(SubFile::new(
            disa[0].clone(),
            fs_info.dir_hash_offset as usize,
            fs_info.dir_buckets as usize * 4,
        )?);

        let file_hash = Rc::new(SubFile::new(
            disa[0].clone(),
            fs_info.file_hash_offset as usize,
            fs_info.file_buckets as usize * 4,
        )?);

        let fat_table = Rc::new(SubFile::new(
            disa[0].clone(),
            fs_info.fat_offset as usize,
            (fs_info.fat_size + 1) as usize * 8,
        )?);

        let data: Rc<RandomAccessFile> = if disa.partition_count() == 2 {
            disa[1].clone()
        } else {
            Rc::new(SubFile::new(
                disa[0].clone(),
                fs_info.data_offset as usize,
                (fs_info.data_block_count * fs_info.block_len) as usize,
            )?)
        };

        let fat = Fat::new(fat_table, data, fs_info.block_len as usize)?;

        let dir_table: Rc<RandomAccessFile> = if disa.partition_count() == 2 {
            Rc::new(SubFile::new(
                disa[0].clone(),
                fs_info.dir_table as usize,
                (fs_info.max_dir + 2) as usize * (SaveExtKey::BYTE_LEN + SaveExtDir::BYTE_LEN + 4),
            )?)
        } else {
            let block = (fs_info.dir_table & 0xFFFF_FFFF) as usize;
            Rc::new(FatFile::open(fat.clone(), block)?)
        };

        let file_table: Rc<RandomAccessFile> = if disa.partition_count() == 2 {
            Rc::new(SubFile::new(
                disa[0].clone(),
                fs_info.file_table as usize,
                (fs_info.max_file + 1) as usize * (SaveExtKey::BYTE_LEN + SaveFile::BYTE_LEN + 4),
            )?)
        } else {
            let block = (fs_info.file_table & 0xFFFF_FFFF) as usize;
            Rc::new(FatFile::open(fat.clone(), block)?)
        };

        let fs = FsMeta::new(dir_hash, dir_table, file_hash, file_table)?;

        Ok(Rc::new(SaveData {
            disa,
            fat,
            fs,
            block_len: fs_info.block_len as usize,
        }))
    }
}

pub struct File {
    center: Rc<SaveData>,
    meta: FileMeta,
    data: Option<FatFile>,
    len: usize,
}

impl File {
    fn from_meta(center: Rc<SaveData>, meta: FileMeta) -> Result<File, Error> {
        let info = meta.get_info()?;
        let len = info.size as usize;
        let data = if info.block == 0x8000_0000 {
            if len != 0 {
                return make_error(Error::SizeMismatch);
            }
            None
        } else {
            let fat_file = FatFile::open(center.fat.clone(), info.block as usize)?;
            if len == 0 || len > fat_file.len() {
                return make_error(Error::SizeMismatch);
            }
            Some(fat_file)
        };
        Ok(File {
            center,
            meta,
            data,
            len,
        })
    }
}

pub struct Dir {
    center: Rc<SaveData>,
    meta: DirMeta,
}

pub struct SaveDataFileSystem {}
impl FileSystem for SaveDataFileSystem {
    type CenterType = SaveData;
    type FileType = File;
    type DirType = Dir;

    fn file_open_ino(center: Rc<Self::CenterType>, ino: u32) -> Result<Self::FileType, Error> {
        let meta = FileMeta::open_ino(center.fs.clone(), ino)?;
        File::from_meta(center, meta)
    }

    fn file_rename(
        file: &mut Self::FileType,
        parent: &Self::DirType,
        name: [u8; 16],
    ) -> Result<(), Error> {
        file.meta.rename(&parent.meta, name)
    }

    fn file_get_parent_ino(file: &Self::FileType) -> u32 {
        file.meta.get_parent_ino()
    }

    fn file_get_ino(file: &Self::FileType) -> u32 {
        file.meta.get_ino()
    }

    fn file_delete(file: Self::FileType) -> Result<(), Error> {
        if let Some(f) = file.data {
            f.delete()?;
        }
        file.meta.delete()
    }

    fn resize(file: &mut Self::FileType, len: usize) -> Result<(), Error> {
        if len == file.len {
            return Ok(());
        }

        let mut info = file.meta.get_info()?;

        if file.len == 0 {
            // zero => non-zero
            let (fat_file, block) = FatFile::create(
                file.center.fat.clone(),
                1 + (len - 1) / file.center.block_len,
            )?;
            file.data = Some(fat_file);
            info.block = block as u32;
        } else if len == 0 {
            // non-zero => zero
            file.data.take().unwrap().delete()?;
            info.block = 0x8000_0000;
        } else {
            file.data
                .as_mut()
                .unwrap()
                .resize(1 + (len - 1) / file.center.block_len)?;
        }

        info.size = len as u64;
        file.meta.set_info(info)?;

        file.len = len;

        Ok(())
    }

    fn read(file: &Self::FileType, pos: usize, buf: &mut [u8]) -> Result<(), Error> {
        if pos + buf.len() > file.len {
            return make_error(Error::OutOfBound);
        }
        file.data.as_ref().unwrap().read(pos, buf)
    }

    fn write(file: &Self::FileType, pos: usize, buf: &[u8]) -> Result<(), Error> {
        if pos + buf.len() > file.len {
            return make_error(Error::OutOfBound);
        }
        file.data.as_ref().unwrap().write(pos, buf)
    }

    fn len(file: &Self::FileType) -> usize {
        file.len
    }

    fn open_root(center: Rc<Self::CenterType>) -> Result<Self::DirType, Error> {
        let meta = DirMeta::open_root(center.fs.clone())?;
        Ok(Dir { center, meta })
    }

    fn dir_open_ino(center: Rc<Self::CenterType>, ino: u32) -> Result<Self::DirType, Error> {
        let meta = DirMeta::open_ino(center.fs.clone(), ino)?;
        Ok(Dir { center, meta })
    }

    fn dir_rename(
        dir: &mut Self::DirType,
        parent: &Self::DirType,
        name: [u8; 16],
    ) -> Result<(), Error> {
        if Self::open_sub_file(&parent, name).is_ok() || Self::open_sub_dir(&parent, name).is_ok() {
            return make_error(Error::AlreadyExist);
        }
        dir.meta.rename(&parent.meta, name)
    }

    fn dir_get_parent_ino(dir: &Self::DirType) -> u32 {
        dir.meta.get_parent_ino()
    }

    fn dir_get_ino(dir: &Self::DirType) -> u32 {
        dir.meta.get_ino()
    }

    fn open_sub_dir(dir: &Self::DirType, name: [u8; 16]) -> Result<Self::DirType, Error> {
        Ok(Dir {
            center: dir.center.clone(),
            meta: dir.meta.open_sub_dir(name)?,
        })
    }

    fn open_sub_file(dir: &Self::DirType, name: [u8; 16]) -> Result<Self::FileType, Error> {
        File::from_meta(dir.center.clone(), dir.meta.open_sub_file(name)?)
    }

    fn list_sub_dir(dir: &Self::DirType) -> Result<Vec<([u8; 16], u32)>, Error> {
        dir.meta.list_sub_dir()
    }

    fn list_sub_file(dir: &Self::DirType) -> Result<Vec<([u8; 16], u32)>, Error> {
        dir.meta.list_sub_file()
    }

    fn new_sub_dir(dir: &Self::DirType, name: [u8; 16]) -> Result<Self::DirType, Error> {
        if Self::open_sub_file(dir, name).is_ok() || Self::open_sub_dir(dir, name).is_ok() {
            return make_error(Error::AlreadyExist);
        }
        let dir_info = SaveExtDir {
            next: 0,
            sub_dir: 0,
            sub_file: 0,
            padding: 0,
        };
        Ok(Dir {
            center: dir.center.clone(),
            meta: dir.meta.new_sub_dir(name, dir_info)?,
        })
    }

    fn new_sub_file(
        dir: &Self::DirType,
        name: [u8; 16],
        len: usize,
    ) -> Result<Self::FileType, Error> {
        if Self::open_sub_file(dir, name).is_ok() || Self::open_sub_dir(dir, name).is_ok() {
            return make_error(Error::AlreadyExist);
        }
        let (fat_file, block) = if len == 0 {
            (None, 0x8000_0000)
        } else {
            let (fat_file, block) =
                FatFile::create(dir.center.fat.clone(), 1 + (len - 1) / dir.center.block_len)?;
            (Some(fat_file), block as u32)
        };
        match dir.meta.new_sub_file(
            name,
            SaveFile {
                next: 0,
                padding1: 0,
                block: block,
                size: len as u64,
                padding2: 0,
            },
        ) {
            Err(e) => {
                if let Some(f) = fat_file {
                    f.delete()?;
                }
                Err(e)
            }
            Ok(meta) => File::from_meta(dir.center.clone(), meta),
        }
    }

    fn dir_delete(dir: Self::DirType) -> Result<(), Error> {
        dir.meta.delete()
    }

    fn commit(center: &Self::CenterType) -> Result<(), Error> {
        center.disa.commit()
    }
}

#[cfg(test)]
mod test {
    use crate::save_data::*;
    #[test]
    fn struct_size() {
        assert_eq!(SaveHeader::BYTE_LEN, 0x20);
        assert_eq!(SaveFile::BYTE_LEN, 24);
    }

}