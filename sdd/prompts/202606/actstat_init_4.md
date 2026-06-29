---
plan: sdd/epics/202606/actstat_init_4.md
---
 Can you help me initialize this repository, which is intended to serve as a way for me to view the status of the most recent GitHub Actions workflow runs for select GitHub projects that I configure?

- The `actstat` tool should be written in Rust since performance is critical (it will likely be used in cronjobs to periodically check on the GitHub Actions workflow statuses of projects that I am actively working on / monitoring).
- These projects should be configured in a config.yml file in the appropriate XDG directory (for example, the ~/.config/actstat/config.yml file should be used on this machine).
- Create the ~/.config/actstat/config.yml file on this machine by creating the equivalent file in my chezmoi repo. I want the following projects configured: all of the projects in the sase-org organization (figure out a nice way to specify "all of the repos in the given organization" using this config file) + all of the projects in the bobs-org organization + the bbugyi200/dotfiles repo + the bbugyi200/actstat repo (this repo).
- The `actstat` command should have one `list` sub-command at first which should be the default command that is run if the `actstat` command is run without sub-commands.
- For every configured project the `actstat list` command should review the most recent N (defaults to 1 but can be specified via the `actstat list` command's `-n|--limit` option) completed GitHub Actions workflows for that project and display a status for each of those workflows.
- If the workflow passed we should show a single line per workflow. If the workflow failed however, we should show useful information for the jobs that failed.
- Make sure this command supports rich human-readable output as well as useful machine-readable output.
- Make sure you write an excellent README.md for this project!
- I want you to lead the design on this one. Make sure you design this feature so it is intuitive, reliable, and (last but not least) beautiful!

This is a large piece of work that should be split into phases. I'll let you decide how many phases to create, but
keep in mind that each phase will be completed by a distinct agent instance (i.e. a distinct `claude` / `agy` /
`codex` / `qwen` / `opencode` command). Think this through thoroughly and create a plan using your `/sase_plan` skill. Submit your plan with the
`sase plan propose` command (as the skill instructs) before making any file changes.

 