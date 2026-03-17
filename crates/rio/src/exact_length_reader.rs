// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! ExactLengthReader: errors if the inner stream reaches EOF before exactly `expected` bytes.
//! Used for decrypted GET so we never send a body shorter than Content-Length (IncompleteBody).

use crate::compress_index::{Index, TryGetIndex};
use crate::{EtagResolvable, HashReaderDetector, HashReaderMut, Reader};
use pin_project_lite::pin_project;
use std::io::{Error, Result};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

pin_project! {
    pub struct ExactLengthReader {
        #[pin]
        pub inner: Box<dyn Reader>,
        expected: i64,
        read_so_far: i64,
    }
}

impl ExactLengthReader {
    pub fn new(inner: Box<dyn Reader>, expected: i64) -> Self {
        ExactLengthReader {
            inner,
            expected,
            read_so_far: 0,
        }
    }
}

impl AsyncRead for ExactLengthReader {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<Result<()>> {
        let this = self.as_mut().project();
        let before = buf.filled().len();

        let poll = this.inner.poll_read(cx, buf);

        if let Poll::Ready(Ok(())) = &poll {
            let after = buf.filled().len();
            let n = (after - before) as i64;
            *this.read_so_far += n;
            // EOF: inner returned Ok(()) but no new bytes
            if n == 0 && *this.read_so_far < *this.expected {
                return Poll::Ready(Err(Error::other(format!(
                    "decryption stream ended early: expected {} bytes, got {}",
                    this.expected, this.read_so_far
                ))));
            }
        }
        poll
    }
}

impl EtagResolvable for ExactLengthReader {
    fn try_resolve_etag(&mut self) -> Option<String> {
        self.inner.try_resolve_etag()
    }
}

impl HashReaderDetector for ExactLengthReader {
    fn is_hash_reader(&self) -> bool {
        self.inner.is_hash_reader()
    }
    fn as_hash_reader_mut(&mut self) -> Option<&mut dyn HashReaderMut> {
        self.inner.as_hash_reader_mut()
    }
}

impl TryGetIndex for ExactLengthReader {
    fn try_get_index(&self) -> Option<&Index> {
        self.inner.try_get_index()
    }
}
