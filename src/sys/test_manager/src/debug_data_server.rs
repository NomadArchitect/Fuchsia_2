// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::ClientEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_test_manager::{DebugData, DebugDataIteratorMarker, DebugDataIteratorRequest};
use futures::{Future, TryStreamExt};
use std::collections::VecDeque;
use vfs::{
    directory::entry::DirectoryEntry, execution_scope::ExecutionScope, file::vmo::read_only_const,
};

pub struct DebugDataFile {
    pub name: String,
    pub contents: Vec<u8>,
}

pub fn serve_debug_data(
    files: Vec<DebugDataFile>,
) -> (ClientEnd<DebugDataIteratorMarker>, impl 'static + Future<Output = ()>) {
    let (client, server) = fidl::endpoints::create_endpoints::<DebugDataIteratorMarker>().unwrap();
    let mut files = files.into_iter().collect::<VecDeque<_>>();

    let fut = async move {
        let scope = ExecutionScope::new();

        let mut stream = server.into_stream().unwrap();

        while let Ok(Some(req)) = stream.try_next().await {
            match req {
                DebugDataIteratorRequest::GetNext { responder, .. } => {
                    let DebugDataFile { name, contents } = match files.pop_front() {
                        Some(value) => value,
                        None => {
                            // Everything has been send. Return empty batch in the iterator.
                            let _ = responder.send(&mut vec![].into_iter());
                            continue;
                        }
                    };

                    let (file_client, file_server) =
                        fidl::endpoints::create_endpoints::<fio::NodeMarker>().unwrap();

                    let file_impl = read_only_const(&contents);
                    std::mem::drop(contents); // contents are copied; release unneeded memory

                    file_impl.open(
                        scope.clone(),
                        fio::OpenFlags::RIGHT_READABLE,
                        0,
                        vfs::path::Path::dot(),
                        file_server,
                    );

                    let mut data_iter = vec![DebugData {
                        name: Some(name),
                        file: Some(fidl::endpoints::ClientEnd::<fio::FileMarker>::new(
                            file_client.into_channel(),
                        )),
                        ..DebugData::EMPTY
                    }]
                    .into_iter();

                    let _ = responder.send(&mut data_iter);
                }
            }
        }
        scope.wait().await;
    };

    (client, fut)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        fuchsia_async as fasync,
        futures::{future::Either, FutureExt},
        std::task::Poll,
    };

    #[fuchsia::test]
    async fn empty_data_returns_empty_repeatedly() {
        let (client, task) = serve_debug_data(vec![]);
        let task = fasync::Task::spawn(task);

        let proxy = client.into_proxy().expect("into proxy");

        let values = proxy.get_next().await.expect("get next");
        assert_eq!(values, vec![]);

        let values = proxy.get_next().await.expect("get next");
        assert_eq!(values, vec![]);

        // Disconnecting stops the serving task.
        std::mem::drop(proxy);
        task.await;
    }

    #[fuchsia::test]
    async fn single_response() {
        let (client, task) = serve_debug_data(vec![DebugDataFile {
            name: "file".to_string(),
            contents: b"test".to_vec(),
        }]);
        let _task = fasync::Task::spawn(task);

        let proxy = client.into_proxy().expect("into proxy");

        let mut values = proxy.get_next().await.expect("get next");
        assert_eq!(1usize, values.len());
        let DebugData { name, file, .. } = values.pop().unwrap();
        assert_eq!(Some("file".to_string()), name);
        let contents = io_util::read_file_bytes(&file.expect("has file").into_proxy().unwrap())
            .await
            .expect("read file");
        assert_eq!(b"test".to_vec(), contents);

        let values = proxy.get_next().await.expect("get next");
        assert_eq!(values, vec![]);
    }

    #[fuchsia::test]
    async fn multiple_responses() {
        let (client, task) = serve_debug_data(vec![
            DebugDataFile { name: "file".to_string(), contents: b"test".to_vec() },
            DebugDataFile { name: "file2".to_string(), contents: b"test2".to_vec() },
        ]);
        let _task = fasync::Task::spawn(task);

        let proxy = client.into_proxy().expect("into proxy");

        // Complete all requests for files before reading from files.
        // This test validates that files continue to be served even when a later GetNext() call
        // comes in.
        let mut responses = vec![];
        responses.push(proxy.get_next().await.expect("get next"));
        responses.push(proxy.get_next().await.expect("get next"));
        for response in &responses {
            assert_eq!(1usize, response.len());
        }

        let responses = futures::future::join_all(
            responses
                .into_iter()
                .flatten()
                .map(|response| async move {
                    let DebugData { name, file, .. } = response;
                    let contents =
                        io_util::read_file_bytes(&file.expect("has file").into_proxy().unwrap())
                            .await
                            .expect("read file");
                    (name.expect("has name"), contents)
                })
                .collect::<Vec<_>>(),
        )
        .await;

        assert_eq!(
            responses,
            vec![("file".to_string(), b"test".to_vec()), ("file2".to_string(), b"test2".to_vec()),]
        );
    }

    #[fuchsia::test]
    fn serve_unfinished_files_after_proxy_closed() {
        let mut executor = fasync::TestExecutor::new().expect("create executor");

        let (client, debug_data_server) = serve_debug_data(vec![
            DebugDataFile { name: "file".to_string(), contents: b"test".to_vec() },
            DebugDataFile { name: "file2".to_string(), contents: b"test2".to_vec() },
        ]);
        let proxy = client.into_proxy().expect("into proxy");
        let debug_data_server = debug_data_server.shared();

        // Complete all requests for files and close the proxy before reading files.
        let get_responses_fut = async move {
            let mut responses = vec![];
            responses.push(proxy.get_next().await.expect("get next"));
            responses.push(proxy.get_next().await.expect("get next"));
            for response in &responses {
                assert_eq!(1usize, response.len());
            }
            assert!(proxy.get_next().await.expect("get next").is_empty());
            drop(proxy);
            responses
        }
        .boxed();

        // Poll both. Server shouldn't terminate yet.
        let mut select_fut = futures::future::select(debug_data_server.clone(), get_responses_fut);
        let responses = match executor.run_until_stalled(&mut select_fut) {
            Poll::Pending => panic!("Expected poll to complete"),
            Poll::Ready(Either::Left(_)) => panic!("Server shouldn't terminate yet"),
            Poll::Ready(Either::Right((responses, _))) => responses,
        };

        // Keep polling server to make sure it doesn't terminate.
        assert!(executor.run_until_stalled(&mut debug_data_server.clone()).is_pending());

        let responses_fut = futures::future::join_all(
            responses
                .into_iter()
                .flatten()
                .map(|response| async move {
                    let DebugData { name, file, .. } = response;
                    let contents =
                        io_util::read_file_bytes(&file.expect("has file").into_proxy().unwrap())
                            .await
                            .expect("read file");
                    (name.expect("has name"), contents)
                })
                .collect::<Vec<_>>(),
        );

        // Both should complete now, and files should be served.
        let mut join_fut = futures::future::join(responses_fut, debug_data_server);
        let responses = match executor.run_until_stalled(&mut join_fut) {
            Poll::Ready((resp, ())) => resp,
            Poll::Pending => panic!("Expected poll to complete"),
        };

        assert_eq!(
            responses,
            vec![("file".to_string(), b"test".to_vec()), ("file2".to_string(), b"test2".to_vec()),]
        );
    }
}
