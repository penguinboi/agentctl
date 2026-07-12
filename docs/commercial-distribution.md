# Commercial distribution gate

The local native-auth mode assumes users install and authenticate each provider
CLI themselves. Before commercial distribution, obtain written confirmation
that the proposed authentication and subscription-rate-limit usage complies
with each provider's current terms. `agentctl` uses its own branding and does
not imitate provider product identity.

Open-source releases are distributed as GitHub-hosted binary archives. All
workspace crates set `publish = false`; neither the CLI nor internal crates are
published to crates.io. A commercial distributor must preserve the license,
document which provider authentication mode it uses, and complete the provider
approval gate before presenting native subscription login as a product feature.
