use crate::read::streaming_peek_iter::fixture_bytes;
use bstr::{BString, ByteSlice};
#[cfg(all(not(feature = "blocking-io"), feature = "async-io"))]
use futures_lite::io::AsyncReadExt;
use git_odb::pack;
use git_packetline::PacketLine;
#[cfg(feature = "blocking-io")]
use std::io::{BufRead, Read};

#[cfg(all(not(feature = "blocking-io"), feature = "async-io"))]
mod util {
    use futures_io::{AsyncBufRead, AsyncRead};
    use futures_lite::{future, AsyncBufReadExt, AsyncReadExt};
    use std::{io::Result, pin::Pin};

    pub struct BlockOn<T>(pub T);

    impl<T: AsyncRead + Unpin> std::io::Read for BlockOn<T> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
            future::block_on(self.0.read(buf))
        }
    }

    impl<T: AsyncBufRead + Unpin> std::io::BufRead for BlockOn<T> {
        fn fill_buf(&mut self) -> Result<&[u8]> {
            future::block_on(self.0.fill_buf())
        }

        fn consume(&mut self, amt: usize) {
            Pin::new(&mut self.0).consume(amt)
        }
    }
}

#[maybe_async::test(feature = "blocking-io", async(feature = "async-io", async_std::test))]
async fn read_pack_with_progress_extraction() -> crate::Result {
    let buf = fixture_bytes("v1/01-clone.combined-output");
    let mut rd = git_packetline::StreamingPeekableIter::new(&buf[..], &[PacketLine::Flush]);

    // Read without sideband decoding
    let mut out = Vec::new();
    rd.as_read().read_to_end(&mut out).await?;
    assert_eq!(out.as_bstr(), b"808e50d724f604f69ab93c6da2919c014667bedb HEAD\0multi_ack thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not deepen-relative no-progress include-tag multi_ack_detailed symref=HEAD:refs/heads/master object-format=sha1 agent=git/2.28.0\n808e50d724f604f69ab93c6da2919c014667bedb refs/heads/master\n".as_bstr());

    let res = rd.read_line().await;
    assert_eq!(
        res.expect("line")??.to_text().expect("data line").0.as_bstr(),
        b"NAK".as_bstr()
    );
    let mut seen_texts = Vec::<BString>::new();
    let mut do_nothing = |is_err: bool, data: &[u8]| {
        assert!(!is_err);
        seen_texts.push(data.as_bstr().into());
    };
    let pack_read = rd.as_read_with_sidebands(&mut do_nothing);
    #[cfg(all(not(feature = "blocking-io"), feature = "async-io"))]
    let pack_entries = pack::data::BytesToEntriesIter::new_from_header(
        util::BlockOn(pack_read),
        pack::data::input::Mode::Verify,
        pack::data::input::EntryDataMode::Ignore,
    )?;
    #[cfg(feature = "blocking-io")]
    let pack_entries = pack::data::BytesToEntriesIter::new_from_header(
        pack_read,
        pack::data::input::Mode::Verify,
        pack::data::input::EntryDataMode::Ignore,
    )?;
    let all_but_last = pack_entries.size_hint().0 - 1;
    let last = pack_entries.skip(all_but_last).next().expect("last entry")?;
    assert_eq!(
        last.trailer
            .expect("trailer to exist on last entry")
            .to_sha1_hex_string(),
        "150a1045f04dc0fc2dbf72313699fda696bf4126"
    );
    assert_eq!(
        seen_texts,
        [
            "Enumerating objects: 3, done.",
            "Counting objects:  33% (1/3)\r",
            "Counting objects:  66% (2/3)\r",
            "Counting objects: 100% (3/3)\r",
            "Counting objects: 100% (3/3), done.",
            "Total 3 (delta 0), reused 0 (delta 0), pack-reused 0"
        ]
        .iter()
        .map(|v| v.as_bytes().as_bstr().to_owned())
        .collect::<Vec<_>>()
    );
    Ok(())
}

#[maybe_async::test(feature = "blocking-io", async(feature = "async-io", async_std::test))]
async fn read_line_trait_method_reads_one_packet_line_at_a_time() -> crate::Result {
    let buf = fixture_bytes("v1/01-clone.combined-output-no-binary");

    let mut rd = git_packetline::StreamingPeekableIter::new(&buf[..], &[PacketLine::Flush]);

    let mut out = String::new();
    let mut r = rd.as_read();
    r.read_line(&mut out).await?;
    assert_eq!(out, "808e50d724f604f69ab93c6da2919c014667bedb HEAD\0multi_ack thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not deepen-relative no-progress include-tag multi_ack_detailed symref=HEAD:refs/heads/master object-format=sha1 agent=git/2.28.0\n");
    out.clear();
    r.read_line(&mut out).await?;
    assert_eq!(out, "808e50d724f604f69ab93c6da2919c014667bedb refs/heads/master\n");
    out.clear();
    r.read_line(&mut out).await?;
    assert_eq!(out, "", "flush means empty lines…");
    out.clear();
    r.read_line(&mut out).await?;
    assert_eq!(out, "", "…which can't be overcome unless the reader is reset");
    assert_eq!(
        r.stopped_at(),
        Some(PacketLine::Flush),
        "it knows what stopped the reader"
    );

    drop(r);
    rd.reset();

    let mut r = rd.as_read();
    r.read_line(&mut out).await?;
    assert_eq!(out, "NAK\n");

    drop(r);

    let mut r = rd.as_read_with_sidebands(|_, _| ());
    out.clear();
    r.read_line(&mut out).await?;
    assert_eq!(out, "&");

    out.clear();
    r.read_line(&mut out).await?;
    assert_eq!(out, "");

    Ok(())
}

#[maybe_async::test(feature = "blocking-io", async(feature = "async-io", async_std::test))]
async fn peek_past_an_actual_eof_is_an_error() -> crate::Result {
    let input = b"0009ERR e";
    let mut rd = git_packetline::StreamingPeekableIter::new(&input[..], &[]);
    let mut reader = rd.as_read();
    let res = reader.peek_data_line().await;
    assert_eq!(res.expect("one line")??, b"ERR e");

    let mut buf = String::new();
    reader.read_line(&mut buf).await?;
    assert_eq!(
        buf, "ERR e",
        "by default ERR lines won't propagate as failure but are merely text"
    );

    let res = reader.peek_data_line().await;
    assert_eq!(
        res.expect("an err").expect_err("foo").kind(),
        std::io::ErrorKind::UnexpectedEof,
        "peeking past the end is not an error as the caller should make sure we dont try 'invalid' reads"
    );
    Ok(())
}

#[maybe_async::test(feature = "blocking-io", async(feature = "async-io", async_std::test))]
async fn peek_past_a_delimiter_is_no_error() -> crate::Result {
    let input = b"0009hello0000";
    let mut rd = git_packetline::StreamingPeekableIter::new(&input[..], &[PacketLine::Flush]);
    let mut reader = rd.as_read();
    let res = reader.peek_data_line().await;
    assert_eq!(res.expect("one line")??, b"hello");

    let mut buf = String::new();
    reader.read_line(&mut buf).await?;
    assert_eq!(buf, "hello");

    let res = reader.peek_data_line().await;
    assert!(
        res.is_none(),
        "peeking past a flush packet is a 'natural' event that shold not cause an error"
    );
    Ok(())
}

#[maybe_async::test(feature = "blocking-io", async(feature = "async-io", async_std::test))]
async fn handling_of_err_lines() {
    let input = b"0009ERR e0009ERR x0000";
    let mut rd = git_packetline::StreamingPeekableIter::new(&input[..], &[]);
    rd.fail_on_err_lines(true);
    let mut buf = [0u8; 2];
    let mut reader = rd.as_read();
    let res = reader.read(buf.as_mut()).await;
    assert_eq!(
        res.unwrap_err().to_string(),
        "e",
        "it respects errors and passes them on"
    );
    let res = reader.read(buf.as_mut()).await;
    assert_eq!(
        res.expect("read to succeed - EOF"),
        0,
        "it stops reading after an error despite there being more to read"
    );
    reader.reset_with(&[PacketLine::Flush]);
    let res = reader.read(buf.as_mut()).await;
    assert_eq!(
        res.unwrap_err().to_string(),
        "x",
        "after a reset it continues reading, but retains the 'fail_on_err_lines' setting"
    );
    assert_eq!(
        reader.stopped_at(),
        None,
        "An error can also be the reason, which is not distinguishable from an EOF"
    );
}