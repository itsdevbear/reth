use crate::p2p::{downloader::Downloader, error::DownloadError};
use futures::Stream;
use reth_primitives::{BlockNumber, SealedBlock, SealedHeader};

/// The block response
#[derive(PartialEq, Eq, Debug)]
pub enum BlockResponse {
    /// Full block response (with transactions or ommers)
    Full(SealedBlock),
    /// The empty block response
    Empty(SealedHeader),
}

impl BlockResponse {
    /// Return the reference to the response header
    pub fn header(&self) -> &SealedHeader {
        match self {
            BlockResponse::Full(block) => &block.header,
            BlockResponse::Empty(header) => header,
        }
    }
}

/// A downloader capable of fetching block bodies from header hashes.
///
/// A downloader represents a distinct strategy for submitting requests to download block bodies,
/// while a [BodiesClient] represents a client capable of fulfilling these requests.
pub trait BodyDownloader:
    Downloader + Stream<Item = Result<BlockResponse, DownloadError>> + Unpin
{
    /// Buffers the bodies from `starting_block` (inclusive) up until `target_block` (inclusive).
    ///
    /// The downloader's stream will always emit bodies in the order they were requested, but
    /// multiple requests may be in flight at the same time.
    ///
    /// The stream may exit early in some cases. Thus, a downloader can only at a minimum guarantee:
    ///
    /// - All emitted bodies map onto a request
    /// - The emitted bodies are emitted in order: i.e. the body for the first block is emitted
    ///   first, even if it was not fetched first.
    ///
    /// It is *not* guaranteed that all the requested bodies are fetched: the downloader may close
    /// the stream before the entire range has been fetched for any reason
    fn buffer_body_requests<'a, 'b, I>(&'a mut self, headers: I)
    where
        I: IntoIterator<Item = &'b SealedHeader>,
        <I as IntoIterator>::IntoIter: Send + 'b,
        'b: 'a;

    /// Returns the current range that the downloader is syncing and will expose over its stream
    fn bodies_in_progress(&self) -> (BlockNumber, BlockNumber);
}
