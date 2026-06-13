# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.10.0](https://git.johnwilger.com/Slipstream/eventcore/compare/eventcore-fs-v0.9.0...eventcore-fs-v0.10.0) - 2026-06-13

### Features

- *(eventcore-types)* glob pattern matching for subscriptions ([#246](https://git.johnwilger.com/Slipstream/eventcore/pulls/246)) ([#410](https://git.johnwilger.com/Slipstream/eventcore/pulls/410))
- *(eventcore)* [**breaking**] streaming reads for read_stream ([#364](https://git.johnwilger.com/Slipstream/eventcore/pulls/364)) ([#414](https://git.johnwilger.com/Slipstream/eventcore/pulls/414))

### Performance

- serialize events once in the append path ([#361](https://git.johnwilger.com/Slipstream/eventcore/pulls/361)) ([#408](https://git.johnwilger.com/Slipstream/eventcore/pulls/408))

## [0.9.0](https://git.johnwilger.com/Slipstream/eventcore/compare/eventcore-fs-v0.8.1...eventcore-fs-v0.9.0) - 2026-06-13

### Features

- *(eventcore-fs)* local-ingestion cursor for projections ([#398](https://git.johnwilger.com/Slipstream/eventcore/pulls/398))
- *(eventcore-fs)* replica-id fingerprint + conflict check ([#400](https://git.johnwilger.com/Slipstream/eventcore/pulls/400))
- *(eventcore-fs)* read-time fsck + dangling-transaction handling ([#402](https://git.johnwilger.com/Slipstream/eventcore/pulls/402))

## [0.8.1](https://git.johnwilger.com/Slipstream/eventcore/compare/eventcore-fs-v0.8.0...eventcore-fs-v0.8.1) - 2026-06-13

### Features

- *(eventcore-fs)* git-mergeable file-based event store backend ([#390](https://git.johnwilger.com/Slipstream/eventcore/pulls/390))
