// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::input::save_input,
    anyhow::{Context as _, Error, Result},
    fidl_fuchsia_fuzzer as fuzz, fuchsia_zircon_status as zx,
    futures::TryStreamExt,
    std::cell::RefCell,
    std::path::{Path, PathBuf},
};

/// Returns which type of corpus is represented by the `fuchsia.fuzzer.Corpus` enum.
///
/// A seed corpus is immutable. A fuzzer can add or modify inputs in its live corpus.
pub fn get_type(seed: bool) -> fuzz::Corpus {
    if seed {
        fuzz::Corpus::Seed
    } else {
        fuzz::Corpus::Live
    }
}

/// Get the corresponding name for a `fuchsia.fuzzer.Corpus` enum.
pub fn get_name(corpus_type: fuzz::Corpus) -> &'static str {
    match corpus_type {
        fuzz::Corpus::Seed => "seed",
        fuzz::Corpus::Live => "live",
        other => unreachable!("unsupported type: {:?}", other),
    }
}

/// Basic corpus information returned by `read`.
#[derive(Debug, PartialEq)]
pub struct CorpusStats {
    pub num_inputs: usize,
    pub total_size: usize,
}

/// Receives and saves inputs from a corpus.
///
/// Takes a `stream` and serves `fuchsia.fuzzer.CorpusReader`. A fuzzer can publish a sequence of
/// test inputs using this protocol, typically in response to a `fuchsia.fuzzer.Controller/Fetch`
/// request or similar. The inputs are saved under `out_dir`, or in the current working directory
/// if `out_dir` is `None.
pub async fn read<P: AsRef<Path>>(
    stream: fuzz::CorpusReaderRequestStream,
    out_dir: Option<P>,
) -> Result<CorpusStats> {
    // Without these `RefCell`s, the compiler will complain about references in the async block
    // below that escape the closure.
    let num_inputs: RefCell<usize> = RefCell::new(0);
    let total_size: RefCell<usize> = RefCell::new(0);

    let out_dir = match out_dir {
        Some(out_dir) => PathBuf::from(out_dir.as_ref()),
        None => std::env::current_dir().context("failed to write to current directory")?,
    };

    stream
        .try_for_each(|request| async {
            match request {
                fuzz::CorpusReaderRequest::Next { test_input, responder } => {
                    {
                        let mut num_inputs = num_inputs.borrow_mut();
                        let mut total_size = total_size.borrow_mut();
                        *num_inputs += 1;
                        *total_size += test_input.size as usize;
                    }
                    let result = match save_input(test_input, &out_dir, None).await {
                        Ok(_) => zx::Status::OK,
                        Err(_) => zx::Status::IO,
                    };
                    responder.send(result.into_raw())
                }
            }
        })
        .await
        .map_err(Error::msg)
        .context("failed to handle fuchsia.fuzzer.CorpusReader request")?;
    let num_inputs = num_inputs.borrow();
    let total_size = total_size.borrow();
    Ok(CorpusStats { num_inputs: *num_inputs, total_size: *total_size })
}

#[cfg(test)]
mod test_fixtures {
    use {
        crate::input::Input,
        anyhow::{Error, Result},
        fidl_fuchsia_fuzzer as fuzz, fuchsia_zircon_status as zx,
    };

    /// Writes a test input using the given `corpus_reader`.
    pub async fn send_one_input(
        corpus_reader: &fuzz::CorpusReaderProxy,
        data: Vec<u8>,
    ) -> Result<()> {
        let (mut fidl_input, input) = Input::create(data)?;
        let (response, _) = futures::try_join!(
            async move { corpus_reader.next(&mut fidl_input).await.map_err(Error::msg) },
            input.send(),
        )?;
        zx::Status::ok(response).map_err(Error::msg)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::test_fixtures::send_one_input,
        super::{get_name, get_type, read, CorpusStats},
        crate::input::test_fixtures::verify_saved,
        crate::util::digest_path,
        crate::util::test_fixtures::Test,
        anyhow::{Error, Result},
        fidl_fuchsia_fuzzer as fuzz,
        futures::join,
    };

    #[test]
    fn test_get_type() -> Result<()> {
        assert_eq!(get_type(true), fuzz::Corpus::Seed);
        assert_eq!(get_type(false), fuzz::Corpus::Live);
        Ok(())
    }

    #[test]
    fn test_get_name() -> Result<()> {
        assert_eq!(get_name(fuzz::Corpus::Seed), "seed");
        assert_eq!(get_name(fuzz::Corpus::Live), "live");
        Ok(())
    }

    #[fuchsia::test]
    async fn test_read() -> Result<()> {
        let test = Test::try_new()?;
        let corpus_dir = test.create_dir("corpus")?;
        let corpus = vec![b"hello".to_vec(), b"world".to_vec(), b"".to_vec()];
        let cloned = corpus.clone();

        let (proxy, stream) =
            fidl::endpoints::create_proxy_and_stream::<fuzz::CorpusReaderMarker>().unwrap();
        let read_fut = read(stream, Some(&corpus_dir));
        let send_fut = || async move {
            for input in corpus.iter() {
                send_one_input(&proxy, input.to_vec()).await?;
            }
            Ok::<(), Error>(())
        };
        let send_fut = send_fut();
        let results = join!(read_fut, send_fut);
        assert_eq!(results.0.ok(), Some(CorpusStats { num_inputs: 3, total_size: 10 }));
        assert!(results.1.is_ok());
        for input in cloned.iter() {
            let saved = digest_path(&corpus_dir, None, input);
            verify_saved(&saved, input)?;
        }
        Ok(())
    }
}
