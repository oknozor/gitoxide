//!
use std::{cell::Ref, convert::TryInto};

use git_hash::ObjectId;
pub use git_object::Kind;

use crate::{
    easy,
    easy::{Object, ObjectRef, TreeRef},
};

mod errors;
pub(crate) mod cache {
    pub use git_pack::cache::object::MemoryCappedHashmap;
}
pub use errors::{conversion, find, write};
mod impls;
pub mod peel;
mod tree;

impl Object {
    /// Infuse this owned object with an [`easy::Handle`].
    pub fn attach(self, handle: &easy::Handle) -> easy::borrow::state::Result<ObjectRef<'_>> {
        *handle.try_borrow_mut_buf()? = self.data;
        Ok(ObjectRef {
            id: self.id,
            kind: self.kind,
            data: Ref::map(handle.try_borrow_buf()?, |v| v.as_slice()),
            handle,
        })
    }
}

impl<'repo> ObjectRef<'repo> {
    pub(crate) fn from_current_buf(
        id: impl Into<ObjectId>,
        kind: Kind,
        handle: &'repo easy::Handle,
    ) -> easy::borrow::state::Result<Self> {
        Ok(ObjectRef {
            id: id.into(),
            kind,
            data: Ref::map(handle.try_borrow_buf()?, |v| v.as_slice()),
            handle,
        })
    }

    /// Transform this object into a tree, or panic if it is none.
    pub fn into_tree(self) -> TreeRef<'repo> {
        match self.try_into() {
            Ok(tree) => tree,
            Err(this) => panic!("Tried to use {} as tree, but was {}", this.id, this.kind),
        }
    }

    /// Transform this object into a tree, or return it as part of the `Err` if it is no tree.
    pub fn try_into_tree(self) -> Result<TreeRef<'repo>, Self> {
        self.try_into()
    }
}

impl<'repo> ObjectRef<'repo> {
    /// Create an owned instance of this object, copying our data in the process.
    pub fn to_owned(&self) -> Object {
        Object {
            id: self.id,
            kind: self.kind,
            data: self.data.to_owned(),
        }
    }

    /// Turn this instance into an owned one, copying our data in the process.
    pub fn into_owned(self) -> Object {
        Object {
            id: self.id,
            kind: self.kind,
            data: self.data.to_owned(),
        }
    }

    /// Sever the connection to `Easy` and turn this instance into a standalone object.
    ///
    /// Note that the data buffer will be copied in the process.
    pub fn detach(self) -> Object {
        self.into()
    }
}

impl<'repo> ObjectRef<'repo> {
    /// Obtain a fully parsed commit whose fields reference our data buffer,
    ///
    /// # Panic
    ///
    /// - this object is not a commit
    /// - the commit could not be decoded
    pub fn to_commit(&self) -> git_object::CommitRef<'_> {
        self.try_to_commit().expect("BUG: need a commit")
    }

    /// Obtain a fully parsed commit whose fields reference our data buffer.
    pub fn try_to_commit(&self) -> Result<git_object::CommitRef<'_>, conversion::Error> {
        git_object::Data::new(self.kind, &self.data)
            .decode()?
            .into_commit()
            .ok_or(conversion::Error::UnexpectedType {
                expected: git_object::Kind::Commit,
                actual: self.kind,
            })
    }

    /// Obtain a an iterator over commit tokens like in [`to_commit_iter()`][ObjectRef::try_to_commit_iter()].
    ///
    /// # Panic
    ///
    /// - this object is not a commit
    pub fn to_commit_iter(&self) -> git_object::CommitRefIter<'_> {
        git_object::Data::new(self.kind, &self.data)
            .try_into_commit_iter()
            .expect("BUG: This object must be a commit")
    }

    /// Obtain a commit token iterator from the data in this instance, if it is a commit.
    pub fn try_to_commit_iter(&self) -> Option<git_object::CommitRefIter<'_>> {
        git_object::Data::new(self.kind, &self.data).try_into_commit_iter()
    }

    /// Obtain a tag token iterator from the data in this instance.
    ///
    /// # Panic
    ///
    /// - this object is not a tag
    pub fn to_tag_iter(&self) -> git_object::TagRefIter<'_> {
        git_object::Data::new(self.kind, &self.data)
            .try_into_tag_iter()
            .expect("BUG: this object must be a tag")
    }

    /// Obtain a tag token iterator from the data in this instance.
    ///
    /// # Panic
    ///
    /// - this object is not a tag
    pub fn try_to_tag_iter(&self) -> Option<git_object::TagRefIter<'_>> {
        git_object::Data::new(self.kind, &self.data).try_into_tag_iter()
    }

    /// Obtain a tag object from the data in this instance.
    ///
    /// # Panic
    ///
    /// - this object is not a tag
    /// - the tag could not be decoded
    pub fn to_tag(&self) -> git_object::TagRef<'_> {
        self.try_to_tag().expect("BUG: need tag")
    }

    /// Obtain a fully parsed tag object whose fields reference our data buffer.
    pub fn try_to_tag(&self) -> Result<git_object::TagRef<'_>, conversion::Error> {
        git_object::Data::new(self.kind, &self.data)
            .decode()?
            .into_tag()
            .ok_or(conversion::Error::UnexpectedType {
                expected: git_object::Kind::Tag,
                actual: self.kind,
            })
    }
}
