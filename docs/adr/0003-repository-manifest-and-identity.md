# ADR 0003: The common Git directory is the state locator

The only repository state locator is `agentree/repository.json` under the
common Git directory. The manifest records the common directory identity and
state paths. An alternate environment-selected database is not accepted.
