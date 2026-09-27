# Share a plugin

The user wants to give a plugin to someone else. Mira has no sharing command: plugins are plain files, and the other person's agent rebuilds them.

## Same repository

Tell the user to commit `.mira/workspace.json` and `.mira/plugins/<id>/`. Keep `.mira/local.json` and `.mira/.drafts/` out of Git.

## Another person or project

Write a prompt that the other person pastes into their own agent. Their agent then builds the same tool, fitted to their project. Put in the prompt:

1. **Purpose:** one sentence on what the tool shows or does, and when to use it.
2. **Shape:** the actions and views, with IDs, kinds (`task`, `process`, table, log, …), inputs, schedules, and persistence.
3. **The source:** the `plugin.json` and any script, in fenced code blocks. Leave out secrets, personal paths, and values from `.mira/local.json`.
4. **What to adapt:** the parts that depend on the project, such as the commands, paths, ports, ticker lists, or channel names. Tell the other agent to read its project and replace these parts, not to copy them blindly.
5. **Checks:** "Use the mira-extend skill. Run `mira validate`, apply the plugin, run each action once, and read each view."

Start the prompt with: "Build this Mira plugin for my project (https://github.com/ImHangLi/mira). If Mira is not installed, follow docs/agents.md first."

Give the user the prompt as one copyable block. Do not send it anywhere yourself.
