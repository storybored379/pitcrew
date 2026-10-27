---
name: Pit Crew owner desk
description: Dense server-rendered operations UI extending the viewer visual system.
colors:
  page: "#191e19"
  surface: "#242b24"
  text: "#eef1eb"
  muted: "#bdc8b8"
  line: "#4c5848"
  control: "#74826e"
  accent: "#b8dca9"
  hover: "#d4edc9"
  focus: "#f1bb70"
  warning: "#f1bb70"
  button-text: "#162411"
typography:
  body:
    fontFamily: "system-ui,sans-serif"
    fontSize: "1rem"
    lineHeight: 1.5
  heading:
    fontSize: "1.85rem"
    lineHeight: 1.2
  section:
    fontSize: "1.25rem"
  brand:
    fontSize: "1.25rem"
    fontWeight: 750
  label:
    fontWeight: 650
  table:
    fontSize: ".925rem"
  evidence:
    fontFamily: "ui-monospace,monospace"
    fontSize: ".85rem"
    lineHeight: 1.5
rounded:
  control: ".35rem"
spacing:
  compact: ".75rem"
  standard: "1rem"
  section: "1.5rem"
components:
  button-primary:
    backgroundColor: "{colors.accent}"
    textColor: "{colors.button-text}"
    rounded: "{rounded.control}"
    padding: ".65rem 1rem"
  button-primary-hover:
    backgroundColor: "{colors.hover}"
  button-quiet:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
    padding: ".65rem 1rem"
  button-disabled:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.muted}"
  input:
    backgroundColor: "{colors.page}"
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
    padding: ".65rem"
  navigation-active:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text}"
    rounded: "{rounded.control}"
    padding: ".7rem .75rem"
  notice:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.text}"
    padding: "{spacing.standard}"
---

# Design System: Pit Crew owner desk

## Overview

This binary extends the established viewer interface with a dense, dark operations
workspace. Forest neutrals, pale green actions, system typography and native HTML
keep saved evidence and explicit action states easy to scan.

This document is scoped to `pitcrew-admin`. The frontmatter records implemented
values from [style.css](templates/style.css); the stylesheet and
[base template](templates/base.html) are the implementation references.
[The viewer stylesheet](../pitcrew-web/templates/style.css) supplies the shared
dark palette. No external fonts, assets or JavaScript are used.

## Colors

Page and surface are dark forest neutrals; text and muted separate primary content
from supporting evidence. Accent marks links, disclosure summaries and primary
actions; hover lightens it. Line separates rows and sections. Control strengthens
input boundaries. Focus and warning share amber, with words communicating warnings.

## Typography

Use the system stack throughout; evidence blocks use the system monospace stack.
Headings establish hierarchy without display typography. Table numbers align with
tabular numerals. Status text and labels use the label weight. Paragraphs are
bounded to 72 characters of measure; evidence wraps long content.

## Layout

The desktop shell has a 13rem navigation column and a flexible content column.
Main content uses fluid horizontal padding; action forms arrange themselves into
columns with a 20rem minimum when space permits. Individual forms remain bounded
to 42rem.

At 60rem and below, navigation becomes a wrapping row above the content. At 36rem
and below, header and table padding tighten and the page heading becomes 1.6rem.
Tables scroll inside their own container; long cell content wraps. Keep navigation
visible and usable on tablets without a scripted menu.

## Elevation & Depth

The interface uses flat surfaces, tonal changes and thin separators. There are no
shadows or decorative overlays. Row hover and selected navigation use surface.

## Shapes

Controls and navigation links share the control radius. Notices and evidence
blocks retain square corners. Dividers and control boundaries are one-pixel
borders. Touch targets use a 44px minimum height on buttons, navigation links,
disclosure summaries and the dedicated content/action links.

## Components

Primary buttons use accent and dark button text; quiet buttons use surface with
a line border. Hover uses the brighter accent; quiet hover also switches text to
page. Disabled buttons use surface, muted text and a line border. Pressing a
button moves it down one pixel; reduced-motion preferences remove that movement.

Fields use visible native labels, page backgrounds and control borders.
Textareas resize vertically. Keyboard focus uses an amber 3px outline offset
by 3px. Preserve the skip link and semantic main/navigation landmarks.

Navigation marks the current page with `aria-current="page"`, surface, primary
text and bold weight. Tables retain captions and header cells, with row hover
helping readers track evidence. Native `details` and `summary` disclose wrapped,
scrollable evidence blocks.

Saved evidence is read only. Native forms submit explicit owner actions with
explanations and audit results. Pending controller integration means the request
was saved; accepted means the API acknowledged it; uncertain means the response
was lost. These labels must remain distinct from a completed effect. Unknown,
stale and unavailable observations stay explicit. See [admin behavior](../../docs/admin.md)
for the action contracts.

## Do's and Don'ts

- Do extend the existing viewer palette and native interaction patterns.
- Do retain visible labels, focus outlines, wrapping navigation and textual states.
- Do make saved evidence and the limits of each action clear near the control.
- Don't infer success or readiness from an accepted or pending request.
- Don't introduce external assets, JavaScript, decorative effects or a new visual identity.
