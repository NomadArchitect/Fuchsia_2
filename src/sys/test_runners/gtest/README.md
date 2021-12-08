# GTest Runner

Reviewed on: 2020-04-20

GTest Runner is a [test runner][test-runner] that launches a gtest binary as a
component, parses its output, and translates it to fuchsia.test.Suite protocol
on behalf of the test.

## Building

```bash
fx set core.x64 --with //src/sys/test_runners/gtest
fx build
```

## Examples

Examples to demonstrate how to write v2 test

-   [Simple test](meta/sample_tests.cml)

To run this example:

```bash
fx test gtest-runner-example-tests
```

## Concurrency

Test cases are executed sequentially by default.
[Instruction to override][override-parallel].

## Arguments

See [passsing-arguments](passing-arguments) to learn more.

## Limitations

-   If a test calls `GTEST_SKIP()`, it will be recorded as `Passed` rather than
    as `Skipped`.
    This is due to a bug in gtest itself.

## Testing

Run:

```bash
fx test gtest_runner_tests
```

## Source layout

The entrypoint is located in `src/main.rs`, the FIDL service implementation and
all the test logic exists in `src/test_server.rs`. Unit tests are co-located
with the implementation.

[test-runner]: ../README.md
[override-parallel]: /docs/concepts/testing/v2/test_component.md#running_test_cases_in_parallel
[passing-arguments]: /docs/concepts/testing/v2/test_runner_framework.md#passing_arguments

