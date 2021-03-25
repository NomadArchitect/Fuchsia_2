# Routing Example

This directory contains an example of [capability
routing](docs/concepts/components/component_manifests#capability-routing) in [Component
Framework](docs/concepts/components/introduction.md)
([Components v2](docs/glossary.md#components-v2)).

## Building

If these components are not present in your build, they can be added by
appending `--with //examples` to your `fx set` command. For example:

```bash
$ fx set core.x64 --with //examples
$ fx build
```

(Disclaimer: if these build rules become out-of-date, please check the
[Build documentation](docs/development/workflows) and update this README!)

## Running

Provide the `echo_realm` component's URL to `run` as an argument to `component_manager`:

```bash
$ fx shell 'run fuchsia-pkg://fuchsia.com/components-routing-example#meta/component_manager_for_examples.cmx fuchsia-pkg://fuchsia.com/components-routing-example#meta/echo_realm.cm'
```

This will run the component in an instance of component manager as a v1
component.

Make sure you have `fx serve` running in another terminal so your component can
be installed!
