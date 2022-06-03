// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::Error,
    fidl_fuchsia_component_resolution as fresolution, fidl_fuchsia_sys as fv1sys,
    fuchsia_async as fasync,
    fuchsia_component::server::ServiceFs,
    fuchsia_component_test::LocalComponentHandles,
    fuchsia_url::AbsoluteComponentUrl,
    futures::{StreamExt, TryStreamExt},
    std::sync::Arc,
    tracing::{error, warn},
};

fn validate_hermetic_package(
    component_url_str: &str,
    hermetic_test_package_name: &String,
) -> Result<(), fresolution::ResolverError> {
    let component_url = AbsoluteComponentUrl::parse(component_url_str)
        .map_err(|_| fresolution::ResolverError::InvalidArgs)?;
    let package_name = component_url.name();
    if hermetic_test_package_name != package_name.as_ref() {
        error!(
                "failed to resolve component {}: package {} is not in the test package: '{}'
                \nSee https://fuchsia.dev/fuchsia-src/development/testing/components/test_runner_framework?hl=en#hermetic-resolver
                for more information.",
                &component_url_str, package_name, hermetic_test_package_name
            );
        return Err(fresolution::ResolverError::PackageNotFound);
    }
    Ok(())
}

pub async fn serve_hermetic_resolver(
    handles: LocalComponentHandles,
    hermetic_test_package_name: Arc<String>,
    universe_resolver: Arc<fresolution::ResolverProxy>,
) -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    let mut tasks = vec![];

    fs.dir("svc").add_fidl_service(move |mut stream: fresolution::ResolverRequestStream| {
        let universe_resolver = universe_resolver.clone();
        let hermetic_test_package_name = hermetic_test_package_name.clone();
        tasks.push(fasync::Task::local(async move {
            while let Some(request) =
                stream.try_next().await.expect("failed to serve component resolver")
            {
                match request {
                    fresolution::ResolverRequest::Resolve { component_url, responder } => {
                        let mut result = if let Err(err) =
                            validate_hermetic_package(&component_url, &hermetic_test_package_name)
                        {
                            Err(err)
                        } else {
                            universe_resolver.resolve(&component_url).await.unwrap_or_else(|err| {
                                error!("failed to resolve component {}: {:?}", component_url, err);
                                Err(fresolution::ResolverError::Internal)
                            })
                        };
                        responder.send(&mut result).expect("failed sending response");
                    }
                    fresolution::ResolverRequest::ResolveWithContext {
                        component_url,
                        context,
                        responder,
                    } => {
                        let mut result = if let Err(err) =
                            validate_hermetic_package(&component_url, &hermetic_test_package_name)
                        {
                            Err(err)
                        } else {
                            universe_resolver
                                .resolve_with_context(&component_url, &context)
                                .await
                                .unwrap_or_else(|err| {
                                    error!(
                                        "failed to resolve component {} with context {:?}: {:?}",
                                        component_url, context, err
                                    );
                                    Err(fresolution::ResolverError::Internal)
                                })
                        };
                        responder.send(&mut result).expect("failed sending response");
                    }
                }
            }
        }));
    });
    fs.serve_connection(handles.outgoing_dir.into_channel())?;
    fs.collect::<()>().await;
    Ok(())
}

async fn hermetic_loader(
    component_url_str: &str,
    hermetic_test_package_name: &String,
    loader_service: fv1sys::LoaderProxy,
) -> Option<Box<fv1sys::Package>> {
    let component_url = match AbsoluteComponentUrl::parse(component_url_str) {
        Ok(u) => u,
        Err(e) => {
            warn!("Invalid component url {}: {}", component_url_str, e);
            return None;
        }
    };
    let package_name = component_url.package_url().name();
    if hermetic_test_package_name != package_name.as_ref() {
        error!(
                "failed to resolve component {}: package {} is not in the test package: '{}'
                \nSee https://fuchsia.dev/fuchsia-src/development/testing/components/test_runner_framework?hl=en#hermetic-resolver
                for more information.",
                &component_url_str, package_name, hermetic_test_package_name
            );
        return None;
    }
    match loader_service.load_url(component_url_str).await {
        Ok(r) => r,
        Err(e) => {
            warn!("can't communicate with global loader: {}", e);
            None
        }
    }
}

pub async fn serve_hermetic_loader(
    mut stream: fv1sys::LoaderRequestStream,
    hermetic_test_package_name: Arc<String>,
    loader_service: fv1sys::LoaderProxy,
) {
    while let Some(fv1sys::LoaderRequest::LoadUrl { url, responder }) =
        stream.try_next().await.expect("failed to serve loader")
    {
        let mut result = hermetic_loader(&url, &hermetic_test_package_name, loader_service.clone())
            .await
            .map(|p| *p);

        responder.send(result.as_mut()).expect("failed sending response");
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        fidl::endpoints::create_proxy_and_stream,
        fuchsia_component_test::error::Error as RealmBuilderError,
        fuchsia_component_test::{
            Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route,
        },
    };

    async fn respond_to_resolve_requests(stream: &mut fresolution::ResolverRequestStream) {
        let request = stream
            .next()
            .await
            .expect("did not get next request")
            .expect("error getting next request");
        match request {
            fresolution::ResolverRequest::Resolve { component_url, responder } => {
                match component_url.as_str() {
                    "fuchsia-pkg://fuchsia.com/package-one#meta/comp.cm" => {
                        responder.send(&mut Ok(fresolution::Component::EMPTY))
                    }
                    "fuchsia-pkg://fuchsia.com/package-two#meta/comp.cm" => {
                        responder.send(&mut Err(fresolution::ResolverError::ResourceUnavailable))
                    }
                    _ => responder.send(&mut Err(fresolution::ResolverError::Internal)),
                }
                .expect("failed sending response");
            }
            fresolution::ResolverRequest::ResolveWithContext {
                component_url,
                context,
                responder,
            } => {
                // This test only responds to Resolve
                error!("this test resolver does not support ResolveWithContext, and could not resolve component URL {:?} with context {:?}", component_url, context);
                responder
                    .send(&mut Err(fresolution::ResolverError::InvalidArgs))
                    .expect("failed sending response");
            }
        }
    }

    async fn respond_to_loader_requests(stream: &mut fv1sys::LoaderRequestStream) {
        let request = stream
            .next()
            .await
            .expect("did not get next request")
            .expect("error getting next request");
        match request {
            fv1sys::LoaderRequest::LoadUrl { url, responder } => {
                match url.as_str() {
                    "fuchsia-pkg://fuchsia.com/package-one#meta/comp.cm" => {
                        responder.send(Some(&mut fv1sys::Package {
                            data: None,
                            directory: None,
                            resolved_url: url,
                        }))
                    }
                    "fuchsia-pkg://fuchsia.com/package-two#meta/comp.cm" => responder.send(None),
                    _ => responder.send(None),
                }
                .expect("failed sending response");
            }
        }
    }

    // Constructs a test realm that contains a local system resolver that we
    // route to our hermetic resolver.
    async fn construct_test_realm(
        hermetic_test_package_name: Arc<String>,
        mock_universe_resolver: Arc<fresolution::ResolverProxy>,
    ) -> Result<RealmInstance, RealmBuilderError> {
        // Set up a realm to test the hermetic resolver.
        let builder = RealmBuilder::new().await?;

        let hermetic_resolver = builder
            .add_local_child(
                "hermetic_resolver",
                move |handles| {
                    Box::pin(serve_hermetic_resolver(
                        handles,
                        hermetic_test_package_name.clone(),
                        mock_universe_resolver.clone(),
                    ))
                },
                ChildOptions::new(),
            )
            .await?;
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fresolution::ResolverMarker>())
                    .from(&hermetic_resolver)
                    .to(Ref::parent()),
            )
            .await?;

        builder.build().await
    }

    #[fuchsia::test]
    async fn test_successful_resolve() {
        let pkg_name = "package-one".to_string();

        let (resolver_proxy, mut resolver_request_stream) =
            create_proxy_and_stream::<fresolution::ResolverMarker>()
                .expect("failed to create mock universe resolver proxy");
        let universe_resolver_task = fasync::Task::spawn(async move {
            respond_to_resolve_requests(&mut resolver_request_stream).await;
            drop(resolver_request_stream);
        });

        let realm = construct_test_realm(pkg_name.into(), Arc::new(resolver_proxy))
            .await
            .expect("failed to construct test realm");
        let hermetic_resolver_proxy =
            realm.root.connect_to_protocol_at_exposed_dir::<fresolution::ResolverMarker>().unwrap();

        assert_eq!(
            hermetic_resolver_proxy
                .resolve("fuchsia-pkg://fuchsia.com/package-one#meta/comp.cm")
                .await
                .unwrap(),
            Ok(fresolution::Component::EMPTY)
        );
        universe_resolver_task.await;
    }

    // Logging disabled as this outputs ERROR log, which will fail the test.
    #[fuchsia::test(logging = false)]
    async fn test_package_not_allowed() {
        let (resolver_proxy, _) = create_proxy_and_stream::<fresolution::ResolverMarker>()
            .expect("failed to create mock universe resolver proxy");

        let realm =
            construct_test_realm("package-two".to_string().into(), Arc::new(resolver_proxy))
                .await
                .expect("failed to construct test realm");
        let hermetic_resolver_proxy =
            realm.root.connect_to_protocol_at_exposed_dir::<fresolution::ResolverMarker>().unwrap();

        assert_eq!(
            hermetic_resolver_proxy
                .resolve("fuchsia-pkg://fuchsia.com/package-one#meta/comp.cm")
                .await
                .unwrap(),
            Err(fresolution::ResolverError::PackageNotFound)
        );
    }

    #[fuchsia::test]
    async fn test_failed_resolve() {
        let (resolver_proxy, mut resolver_request_stream) =
            create_proxy_and_stream::<fresolution::ResolverMarker>()
                .expect("failed to create mock universe resolver proxy");
        let universe_resolver_task = fasync::Task::spawn(async move {
            respond_to_resolve_requests(&mut resolver_request_stream).await;
            drop(resolver_request_stream);
        });

        let pkg_name = "package-two".to_string();

        let realm = construct_test_realm(pkg_name.into(), Arc::new(resolver_proxy))
            .await
            .expect("failed to construct test realm");
        let hermetic_resolver_proxy =
            realm.root.connect_to_protocol_at_exposed_dir::<fresolution::ResolverMarker>().unwrap();

        assert_eq!(
            hermetic_resolver_proxy
                .resolve("fuchsia-pkg://fuchsia.com/package-two#meta/comp.cm")
                .await
                .unwrap(),
            Err(fresolution::ResolverError::ResourceUnavailable)
        );
        universe_resolver_task.await;
    }

    #[fuchsia::test]
    async fn test_invalid_url() {
        let (resolver_proxy, _) = create_proxy_and_stream::<fresolution::ResolverMarker>()
            .expect("failed to create mock universe resolver proxy");

        let pkg_name = "package-two".to_string();

        let realm = construct_test_realm(pkg_name.into(), Arc::new(resolver_proxy))
            .await
            .expect("failed to construct test realm");
        let hermetic_resolver_proxy =
            realm.root.connect_to_protocol_at_exposed_dir::<fresolution::ResolverMarker>().unwrap();

        assert_eq!(
            hermetic_resolver_proxy.resolve("invalid_url").await.unwrap(),
            Err(fresolution::ResolverError::InvalidArgs)
        );
    }

    mod loader {
        use super::*;

        #[fuchsia::test]
        async fn test_successful_loader() {
            let pkg_name = "package-one".to_string();

            let (loader_proxy, mut loader_request_stream) =
                create_proxy_and_stream::<fv1sys::LoaderMarker>()
                    .expect("failed to create mock loader proxy");
            let (proxy, stream) = create_proxy_and_stream::<fv1sys::LoaderMarker>().unwrap();

            let _loader_task = fasync::Task::spawn(async move {
                respond_to_loader_requests(&mut loader_request_stream).await;
                drop(loader_request_stream);
            });

            let _serve_task =
                fasync::Task::spawn(serve_hermetic_loader(stream, pkg_name.into(), loader_proxy));

            assert_matches!(
                proxy.load_url("fuchsia-pkg://fuchsia.com/package-one#meta/comp.cm").await.unwrap(),
                Some(_)
            );
        }

        // Logging disabled as this outputs ERROR log, which will fail the test.
        #[fuchsia::test(logging = false)]
        async fn test_package_not_allowed() {
            let (loader_proxy, _) = create_proxy_and_stream::<fv1sys::LoaderMarker>()
                .expect("failed to create mock loader proxy");

            let (proxy, stream) = create_proxy_and_stream::<fv1sys::LoaderMarker>().unwrap();

            let _serve_task = fasync::Task::spawn(serve_hermetic_loader(
                stream,
                "package-two".to_string().into(),
                loader_proxy,
            ));

            assert_eq!(
                proxy.load_url("fuchsia-pkg://fuchsia.com/package-one#meta/comp.cm").await.unwrap(),
                None
            );
        }

        #[fuchsia::test]
        async fn test_failed_loader() {
            let (loader_proxy, mut loader_request_stream) =
                create_proxy_and_stream::<fv1sys::LoaderMarker>()
                    .expect("failed to create mock loader proxy");
            let _loader_task = fasync::Task::spawn(async move {
                respond_to_loader_requests(&mut loader_request_stream).await;
                drop(loader_request_stream);
            });

            let pkg_name = "package-two".to_string();

            let (proxy, stream) = create_proxy_and_stream::<fv1sys::LoaderMarker>().unwrap();

            let _serve_task =
                fasync::Task::spawn(serve_hermetic_loader(stream, pkg_name.into(), loader_proxy));

            assert_eq!(
                proxy.load_url("fuchsia-pkg://fuchsia.com/package-two#meta/comp.cm").await.unwrap(),
                None
            );
        }

        #[fuchsia::test]
        async fn test_invalid_url_loader() {
            let (loader_proxy, _) = create_proxy_and_stream::<fv1sys::LoaderMarker>()
                .expect("failed to create mock loader proxy");

            let pkg_name = "package-two".to_string();

            let (proxy, stream) = create_proxy_and_stream::<fv1sys::LoaderMarker>().unwrap();

            let _serve_task =
                fasync::Task::spawn(serve_hermetic_loader(stream, pkg_name.into(), loader_proxy));

            assert_eq!(proxy.load_url("invalid_url").await.unwrap(), None);
        }
    }
}
