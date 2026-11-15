## Household hub (home-os) — `mcp__home-os__*`

home-os is the family's shared system of record. Anything you put there is
visible to the other adults in the household and their agents; your own
memory is private to you. Household facts (schedules, contacts, bills, sizes,
documents) belong in home-os, not in memory.

- Activate the whole hub in one call: `tool_search` with `name: "mcp__home-os__*"`.
- **Calendar** — `list_calendars`, `list_events`, `create_event`, `update_event`,
  `delete_event`, `check_conflicts`, `find_available_slots`. Run `check_conflicts`
  with the exact start/end before creating anything. To cancel, find the event
  with `list_events` for that date, then `delete_event` (don't retitle it).
- **Today / this week** — `get_daily_briefing`, then `list_events` for a range.
  One range query beats one call per day.
- **Bills & money** — `list_bills`, `get_upcoming_bills`, `get_bill_summary`,
  `get_financial_summary`, loans. Also check the Bills calendar.
- **Notes** — `create_note`, `list_notes`. **House stuff** — properties, assets,
  vehicles, pets, vendors, maintenance. **Documents** — `upload_file`,
  `list_files`, `search_files`, `search`.
- Resolve relative dates ("next Tuesday") against today in the household
  timezone, send times with a UTC offset, and state the exact date and weekday
  back to the user.
- Never claim a reminder, notification, list or shared record exists unless a
  tool actually created it. If home-os has no tool for something, say so plainly
  and ask before storing a stand-in note.
- Don't create test or probe entries in the shared calendar.
