use crate::object::{self, SHA1_SIZE};
use byteorder::{BigEndian, ByteOrder};
use filebuffer::FileBuffer;
use std::{mem::size_of, path::Path};

const V2_SIGNATURE: &[u8] = b"\xfftOc";
const FOOTER_SIZE: usize = SHA1_SIZE * 2;
const N32_SIZE: usize = size_of::<u32>();
const N64_SIZE: usize = size_of::<u64>();
const FAN_LEN: usize = 256;
const V1_HEADER_SIZE: usize = FAN_LEN * N32_SIZE;
const V2_HEADER_SIZE: usize = N32_SIZE * 2 + FAN_LEN * N32_SIZE;
const N32_HIGH_BIT: u32 = 1 << 31;

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        Io(err: std::io::Error, path: std::path::PathBuf) {
            display("Could not open pack index file at '{}'", path.display())
            cause(err)
        }
        Corrupt(msg: String) {
            display("{}", msg)
        }
        UnsupportedVersion(version: u32) {
            display("Unsupported index version: {}", version)
        }
    }
}

#[derive(PartialEq, Eq, Debug, Hash, Clone)]
pub enum Kind {
    V1,
    V2,
}

impl Default for Kind {
    fn default() -> Self {
        Kind::V2
    }
}

#[derive(PartialEq, Eq, Debug, Hash, Clone)]
pub struct Entry {
    pub oid: object::Id,
    pub offset: u64,
    pub crc32: Option<u32>,
}

pub struct File {
    data: FileBuffer,
    kind: Kind,
    version: u32,
    num_objects: u32,
    _fan: [u32; FAN_LEN],
}

impl File {
    pub fn kind(&self) -> Kind {
        self.kind.clone()
    }
    pub fn num_objects(&self) -> u32 {
        self.num_objects
    }
    pub fn version(&self) -> u32 {
        self.version
    }
    pub fn checksum_of_index(&self) -> object::Id {
        object::id_from_20_bytes(&self.data[self.data.len() - SHA1_SIZE..])
    }
    pub fn checksum_of_pack(&self) -> object::Id {
        let from = self.data.len() - SHA1_SIZE * 2;
        object::id_from_20_bytes(&self.data[from..from + SHA1_SIZE])
    }

    fn offset_crc32_v2(&self) -> usize {
        V2_HEADER_SIZE + self.num_objects as usize * SHA1_SIZE
    }

    fn offset_pack_offset_v2(&self) -> usize {
        self.offset_crc32_v2() + self.num_objects as usize * N32_SIZE
    }

    fn offset_pack_offset64_v2(&self) -> usize {
        self.offset_pack_offset_v2() + self.num_objects as usize * N32_SIZE
    }

    fn iter_v1<'a>(&'a self) -> Result<impl Iterator<Item = Entry> + 'a, Error> {
        Ok(match self.kind {
            Kind::V1 => self.data[V1_HEADER_SIZE..]
                .chunks(N32_SIZE + SHA1_SIZE)
                .take(self.num_objects as usize)
                .map(|c| {
                    let (ofs, oid) = c.split_at(N32_SIZE);
                    Entry {
                        oid: object::id_from_20_bytes(oid),
                        offset: BigEndian::read_u32(ofs) as u64,
                        crc32: None,
                    }
                }),
            _ => unreachable!("Cannot use iter_v1() on index of type {:?}", self.kind),
        })
    }

    fn iter_v2<'a>(&'a self) -> Result<impl Iterator<Item = Entry> + 'a, Error> {
        let pack64_offset = self.offset_pack_offset64_v2();
        Ok(match self.kind {
            Kind::V2 => izip!(
                self.data[V2_HEADER_SIZE..].chunks(SHA1_SIZE),
                self.data[self.offset_crc32_v2()..].chunks(N32_SIZE),
                self.data[self.offset_pack_offset_v2()..].chunks(N32_SIZE)
            )
            .take(self.num_objects as usize)
            .map(move |(oid, crc32, ofs32)| Entry {
                oid: object::id_from_20_bytes(oid),
                offset: {
                    let ofs32 = BigEndian::read_u32(ofs32);
                    if (ofs32 & N32_HIGH_BIT) == N32_HIGH_BIT {
                        let from = pack64_offset + (ofs32 ^ N32_HIGH_BIT) as usize * N64_SIZE;
                        BigEndian::read_u64(&self.data[from..from + N64_SIZE])
                    } else {
                        ofs32 as u64
                    }
                },
                crc32: Some(BigEndian::read_u32(crc32)),
            }),
            _ => unreachable!("Cannot use iter_v2() on index of type {:?}", self.kind),
        })
    }

    pub fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = Entry> + 'a> {
        match self.kind {
            Kind::V1 => Box::new(self.iter_v1().expect("correct check")),
            Kind::V2 => Box::new(self.iter_v2().expect("correct check")),
        }
    }

    pub fn at(path: impl AsRef<Path>) -> Result<File, Error> {
        let data =
            FileBuffer::open(path.as_ref()).map_err(|e| Error::Io(e, path.as_ref().to_owned()))?;
        let idx_len = data.len();
        if idx_len < FAN_LEN * N32_SIZE + FOOTER_SIZE {
            return Err(Error::Corrupt(format!(
                "Pack index of size {} is too small for even an empty index",
                idx_len
            )));
        }
        let (kind, version, fan, num_objects) = {
            let (kind, d) = {
                let (sig, d) = data.split_at(V2_SIGNATURE.len());
                if sig == V2_SIGNATURE {
                    (Kind::V2, d)
                } else {
                    (Kind::V1, &data[..])
                }
            };
            let (version, d) = {
                let (mut v, mut d) = (1, d);
                if let Kind::V2 = kind {
                    let (vd, dr) = d.split_at(N32_SIZE);
                    d = dr;
                    v = BigEndian::read_u32(vd);
                    if v != 2 {
                        return Err(Error::UnsupportedVersion(v));
                    }
                }
                (v, d)
            };
            let (fan, bytes_read) = read_fan(d);
            let (_, _d) = d.split_at(bytes_read);
            let num_objects = fan[FAN_LEN - 1];

            (kind, version, fan, num_objects)
        };
        Ok(File {
            data,
            kind,
            num_objects,
            version,
            _fan: fan,
        })
    }
}

fn read_fan(d: &[u8]) -> ([u32; FAN_LEN], usize) {
    let mut fan = [0; FAN_LEN];
    for (c, f) in d.chunks(N32_SIZE).zip(fan.iter_mut()) {
        *f = BigEndian::read_u32(c);
    }
    (fan, FAN_LEN * N32_SIZE)
}
