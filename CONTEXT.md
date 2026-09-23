# Context

## Glossary

- Tab: a single page a user opens, views, switches between, and closes.
- Tab state: lifecycle stage of a tab. Values: active, standby, idle, sleeping, parked.
  Sleeping releases the tab's web process after a long idle period even without
  memory pressure; parked does the same under pressure. Both keep the tab listed
  and restore on click.
- Policy: rule set that decides when a tab changes state.
- Exemption: user visible reason that stops freeze or park, such as sound, a call, a download, unsaved input, or a pin.
- Freeze: reversible pause of background work in a tab that keeps the page in memory.
- Park: reversible release of a tab from memory that keeps its identity in the tab strip.
- Restore: act that brings a parked tab back to full use.
- Process pool: helpers that run page content outside the main window.
- Blocker: filter that stops ads and trackers before they load.
- Session: saved set of open tabs that survives a restart or crash.
- Bookmark: user saved page (star button, Ctrl+D) that survives restarts and shows on the start page.
- Frequent: start page section ranked by visit count, excluding internal and search result pages.
- Suggestion: address bar dropdown row drawn from history while typing.
- Thumbnail: small static preview shown while a parked tab restores.

## States

- Active: tab shows on screen now.
- Standby: tab left the screen recently and keeps full behavior.
- Idle: tab stayed unseen for minutes and runs with reduced background work.
- Parked: tab stays listed but holds no live page until the user returns.
