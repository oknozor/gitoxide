use std::convert::TryInto;

use git_hash::oid;
use git_object::Data;
use git_pack::find::Entry;

use crate::{
    pack,
    store::{compound, linked},
};

impl crate::pack::Find for linked::Store {
    type Error = compound::find::Error;

    /// Return true if the given object `id` is contained in the store.
    fn contains(&self, id: impl AsRef<oid>) -> bool {
        let id = id.as_ref();
        for db in self.dbs.iter() {
            if db.internal_find_packed(id).is_some() || db.loose.contains(id) {
                return true;
            }
        }
        false
    }

    fn try_find_cached<'a>(
        &self,
        id: impl AsRef<oid>,
        buffer: &'a mut Vec<u8>,
        pack_cache: &mut impl git_pack::cache::DecodeEntry,
    ) -> Result<Option<(git_object::Data<'a>, Option<pack::bundle::Location>)>, Self::Error> {
        let id = id.as_ref();
        for db in self.dbs.iter() {
            match db.internal_find_packed(id) {
                Some(compound::find::PackLocation {
                    bundle_index: pack_id,
                    entry_index,
                }) => {
                    return db
                        .internal_get_packed_object_by_index(pack_id, entry_index, buffer, pack_cache)
                        .map(|(obj, location)| Some((obj, Some(location))))
                        .map_err(Into::into);
                }
                None => {
                    if db.loose.contains(id) {
                        return db
                            .loose
                            .try_find(id, buffer)
                            .map(|o| o.map(|o| (o, None)))
                            .map_err(Into::into);
                    }
                }
            }
        }
        Ok(None)
    }

    fn location_by_oid(&self, id: impl AsRef<oid>, buf: &mut Vec<u8>) -> Option<pack::bundle::Location> {
        let id = id.as_ref();
        for db in self.dbs.iter() {
            if let Some(compound::find::PackLocation {
                bundle_index,
                entry_index,
            }) = db.internal_find_packed(id)
            {
                let bundle = &db.bundles[bundle_index];
                let pack_offset = bundle.index.pack_offset_at_index(entry_index);
                let entry = bundle.pack.entry(pack_offset);

                buf.resize(entry.decompressed_size.try_into().expect("representable size"), 0);
                return bundle
                    .pack
                    .decompress_entry(&entry, buf)
                    .ok()
                    .map(|entry_size_past_header| pack::bundle::Location {
                        pack_id: bundle.pack.id,
                        pack_offset,
                        index_file_id: entry_index,
                        entry_size: entry.header_size() + entry_size_past_header,
                    });
            }
        }
        None
    }

    fn index_iter_by_pack_id(&self, pack_id: u32) -> Option<Box<dyn Iterator<Item = git_pack::index::Entry> + '_>> {
        self.dbs.iter().find_map(|db| {
            db.bundles
                .iter()
                .find_map(|b| (b.pack.id == pack_id).then(|| b.index.iter()))
        })
    }

    fn entry_by_location(&self, location: &pack::bundle::Location) -> Option<Entry<'_>> {
        self.dbs
            .iter()
            .find_map(|db| db.bundles.iter().find(|p| p.pack.id == location.pack_id))
            .map(|b| (b, location))
            .and_then(|(bundle, l)| {
                let crc32 = bundle.index.crc32_at_index(l.index_file_id);
                let pack_offset = bundle.index.pack_offset_at_index(l.index_file_id);
                bundle.pack.entry_slice(l.entry_range(pack_offset)).map(|data| Entry {
                    data,
                    crc32,
                    version: bundle.pack.version(),
                })
            })
    }
}

impl crate::Find for linked::Store {
    type Error = compound::find::Error;

    fn contains(&self, id: impl AsRef<oid>) -> bool {
        pack::Find::contains(self, id)
    }

    fn try_find<'a>(&self, id: impl AsRef<oid>, buffer: &'a mut Vec<u8>) -> Result<Option<Data<'a>>, Self::Error> {
        pack::Find::try_find(self, id, buffer).map(|t| t.map(|t| t.0))
    }
}
