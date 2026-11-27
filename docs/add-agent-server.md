# "Agent hinzufügen": what the account and Wave still have to offer

The menu entry (docs/menu.md) goes as far as the interfaces allow today. This page says exactly what is missing so the entry can become one
step: pick a kind, name it, sign in, done. Nothing here is built; it is the spec of the server part.

## What exists today

| Step | How it works now | Who can do it |
|---|---|---|
| Make an agent in Wave | `POST /v1/companies/:company/team/agents` with `{team, kind: "wave" \| "own", name, owner?, access?}` | A person in the Wave app, with the app's own token (channel `wave`). **Not** the CLI's sign-in token. |
| Open the connect window | `POST /v1/companies/:company/team/agents/:agent/connect-window` (15 minutes) | The person the agent belongs to, or an admin, in the Wave app |
| Start a sign-in for an agent | `POST /v1/device/code` with `purpose: "agent"` and `want: {engine, agent_name?, company?}` | The CLI. `want` is only a **suggestion** for the confirmation page, never a right; unknown fields are dropped. |
| Confirm | The account's `/link` page: four words, the person picks one of the **agents with an open window**, types its name | The person, in the browser |
| Get the key | `POST /v1/agent-connect/mint` with the sign-in token (`cli_agent`, 15 minutes) | The CLI; the key is shown once and only to this device |

The menu can therefore not make the agent. It sends the name as `want.agent_name` and tells the person to make the agent and open the window in Wave
first.

## A. Claude Code, Codex, Grok: make the agent on the confirmation page

Goal: the person never leaves the sign-in. Recommended shape (no new token scope):

1. `want.create: true` and an optional `want.team` in `POST /v1/device/code` (the account's `cleanWant` keeps them; both are only suggestions).
2. For such a code the confirmation page shows a choice "Neuen Agenten anlegen" next to the existing agents: team (the person's teams; an admin sees
   all), the name (prefilled from `want.agent_name`), owner (`me` / `company`, as in the Team API).
3. On approval the account runs what `POST .../team/agents` runs (the same checks: owner/admin rights, `own_agents_per_team`, personal agents on or
   off), opens the connect window for the new agent, binds the code to it and records all of it in the audit log (with the way it was done: by device code). The existing second tick and the typed-name check stay; for a new agent the page asks to type the new name instead of the
   name of an agent whose key would be replaced.
4. `mint` works as today. No new token scope, no Team-API access for the CLI.

Rejected: giving the CLI a token for the Team API (a second token kind with its own binding to the device, wider than "make one agent now").

Open question for the account (not decided here): whether an extra own agent in a team changes the team's price or size; the page must say so before
the tick if it does.

## B. Gemini (gehostet): three things are missing

A hosted agent is run by Wave for the company. Nothing of it runs on the person's computer.

1. **The kind `hosted`.** `kind: "wave" | "own"` becomes `"wave" | "own" | "hosted"` in the Team API and in what the confirmation page can make
   (admin only, like `wave`). Today a hosted agent exists only because an operator switches an agent's engine to `hosted` with the Wave API's admin
   command; there is no endpoint.
2. **Prepare the machine the agent runs on.** Wave needs an endpoint that makes (or reserves) the isolated machine for the agent and reports it:
   `GET /v1/agent-connect/hosted/:agent/status` -> `{machine: "preparing" | "ready" | "failed", login: {state: "none" | "waiting" | "done", url?, code?}}`.
   Today the machine is set up by hand by an operator.
3. **The vendor sign-in, finished on that machine.** For Gemini this is a Google sign-in whose result must stay on the agent's machine.
   `POST /v1/agent-connect/hosted/:agent/login/start` makes the machine start its sign-in and puts the link (and a code, if the vendor uses one)
   into `login`. The menu shows them, the person signs in in the browser, the menu waits for `login.state = "done"`. The key of the vendor never
   passes through the account or the menu.

Decisions that are not interfaces (they belong to the company, not to this menu): where hosted agents run, whose vendor account signs in (the
person's own or one of the company), and who pays for their use. Until they are made the menu says what is missing and starts nothing.

## What the menu does once A exists

- The "Sign in" screen drops the sentence "Angelegt wird der Agent in Wave" and sends `want.create`.
- Name and kind are already collected in the order above, so nothing in the flow changes for the person.
