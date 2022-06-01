# Root Presenter

This component is being deprecated, with:

- [Input Pipeline](./../input-pipeline/README.md) managing input device lifecycle and lower-level
  input event dispatch, and
- [Scene Manager](./../scene_manager/README.md)) creating the root of the global scene graph and
  connecting root-level Views by clients such as Sys UI.

Once the above features in root presenter are replaced, this component still provides virtual
keyboard functionality.

Please reach out to the OWNERS to coordinate any intended work related to this component and its
current or future responsibilities.

## Usage

This program is a server, and so is not started directly. See the `present_view` tool.

## CML file and integration tests

Note that the `meta/` directory has two CML files. One is for production, the
other for tests.

The production package `//src/ui/bin/root_presenter:root_presenter` includes
`meta/root_presenter.cml`, which exists to serves routes related to scene ownership
and virtual keyboard input.

Test packages should include `//src/ui/bin/root_presenter:component_v2_for_test`
and launch it with `fuchsia-pkg://fuchsia.com/<your-test-package>#meta/root_presenter.cm`
for tests which rely on Root Presenter's input functionality. This test-only Root
Presenter provides the `fuchsia.ui.input.InputDeviceRegistry` capability
on top of the production package in order to enable input ownership in Root Presenter.

Generally, test packages should include their own copy of a component to ensure
hermeticity with respect to package loading semantics.

Integration tests don't require access to the device files, because (1) input
injection occurs at a different protocol in Root Presenter, and (2) exposure to
the actual device files is a flake liability for these tests.

During regular maintenance, when adding a new service dependency, add it to
`meta/root_presenter.cml`, so that it is seen in both tests and production.
