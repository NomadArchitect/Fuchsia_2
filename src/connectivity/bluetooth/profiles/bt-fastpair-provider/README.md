# Bluetooth Profile: Fast Pair Provider

This component implements the Google Fast Pair Service (GFPS) Provider role as defined in the
[official specification](https://developers.google.com/nearby/fast-pair/spec).

## Build Configuration

Ensure `//src/connectivity/bluetooth/profiles/bt-fastpair-provider` is in your Fuchsia build. To
include it in the universe set of packages, use the `fx set` configuration or `fx args`. To include
it in the base or cached set of packages, update the product-specific `.gni` file.

## Testing

Add the following to your Fuchsia configuration to include the component unit tests in your build:

`//src/connectivity/bluetooth/profiles/bt-fastpair-provider:bt-fastpair-provider-tests`

To run the tests:

```
fx test bt-fastpair-provider-tests
```
