# Security policy

## Reporting a vulnerability

Please report a security problem **privately**, not in a public issue: open this
repository's **Security** tab and click **Report a vulnerability**
([direct link](../../security/advisories/new)). Only the maintainer sees the report.

Include, if you can:

- the Glyde version and your system;
- the file that triggers the problem, or how to make one;
- what happens, and what you expected instead.

A confirmed problem is fixed in a new release as soon as possible, and the advisory is
published with that release.

## What counts as a vulnerability

Glyde is a desktop app that opens files on your computer. It makes no network
connections, and it opens your files read-only. A vulnerability is anything that breaks
that, for example a file that makes Glyde:

- run code, or read or write anything other than the file you opened and Glyde's own
  cache and log folders;
- change the file you opened.

A problem in how the downloadable files are built or published counts too.

A file that only makes Glyde crash, hang or use too much memory is a bug, not a
vulnerability: open a normal issue with the **"My file won't open"** template. The known
cases are listed under *Known limitations* in the [README](README.md#known-limitations-in-v01).

## Supported versions

Only the latest release receives fixes.

## Checking a download

Every release has a `SHA256SUMS` file, and a signed record (a build provenance
attestation) showing that each file was built by this repository's CI. How to check
both: [Check your download](README.md#check-your-download-optional).

## More

The threat model, what was reviewed before v0.1 and how Glyde is fuzzed:
[`docs/dev/security.md`](docs/dev/security.md).
