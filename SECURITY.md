# Security Policy

## Reporting a vulnerability

If you find a security issue, please report it privately rather than opening a public
issue.

Use GitHub's private vulnerability reporting:

1. Open the **Security** tab of the repository.
2. Click **Report a vulnerability**.
3. Include a description, affected version/commit, and a reproduction if you have one.

We'll acknowledge your report and work with you on a fix and disclosure timeline.

## Supported versions

This project is pre-1.0 and under active development; all crates are currently `0.0.0`
and not yet published to crates.io. Fixes land on the default branch (`master`). A
supported-versions table will be added once releases begin.

## Note

The vendored [`rive-runtime`](https://github.com/rive-app/rive-runtime) is third-party
code included as a submodule; issues in Rive's renderer itself are best reported
upstream. Issues in this project's own code belong here.
