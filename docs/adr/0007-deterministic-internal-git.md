# ADR 0007: Internal Git uses deterministic profiles

Internal mutations use typed profiles that disable hooks, fsmonitor, automatic
maintenance, autostash, and shared rerere where relevant. User configuration
cannot silently enable a second branch update. All arguments are passed as
argv values; no shell command string is constructed.
