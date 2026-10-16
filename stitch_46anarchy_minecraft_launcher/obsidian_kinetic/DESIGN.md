---
name: Obsidian Kinetic
colors:
  surface: '#131313'
  surface-dim: '#131313'
  surface-bright: '#393939'
  surface-container-lowest: '#0e0e0e'
  surface-container-low: '#1c1b1b'
  surface-container: '#201f1f'
  surface-container-high: '#2a2a2a'
  surface-container-highest: '#353534'
  on-surface: '#e5e2e1'
  on-surface-variant: '#e4beb1'
  inverse-surface: '#e5e2e1'
  inverse-on-surface: '#313030'
  outline: '#ab897d'
  outline-variant: '#5b4137'
  surface-tint: '#ffb59a'
  primary: '#ffb59a'
  on-primary: '#5a1b00'
  primary-container: '#ff5c00'
  on-primary-container: '#521800'
  inverse-primary: '#a73a00'
  secondary: '#ffdb9d'
  on-secondary: '#412d00'
  secondary-container: '#feb700'
  on-secondary-container: '#6b4b00'
  tertiary: '#a0c9ff'
  on-tertiary: '#00325a'
  tertiary-container: '#0096fd'
  on-tertiary-container: '#002d51'
  error: '#ffb4ab'
  on-error: '#690005'
  error-container: '#93000a'
  on-error-container: '#ffdad6'
  primary-fixed: '#ffdbce'
  primary-fixed-dim: '#ffb59a'
  on-primary-fixed: '#370e00'
  on-primary-fixed-variant: '#802a00'
  secondary-fixed: '#ffdea8'
  secondary-fixed-dim: '#ffba20'
  on-secondary-fixed: '#271900'
  on-secondary-fixed-variant: '#5e4200'
  tertiary-fixed: '#d2e4ff'
  tertiary-fixed-dim: '#a0c9ff'
  on-tertiary-fixed: '#001c37'
  on-tertiary-fixed-variant: '#00497f'
  background: '#131313'
  on-background: '#e5e2e1'
  surface-variant: '#353534'
  surface-obsidian: '#0D0D0E'
  surface-card: '#18181A'
  surface-elevated: '#222226'
  border-subtle: '#2B2B30'
  border-focus: '#FF5C00'
  text-primary: '#FFFFFF'
  text-muted: '#8A8A93'
  status-online: '#00E575'
  status-alert: '#FF334B'
typography:
  display:
    fontFamily: Montserrat
    fontSize: 32px
    fontWeight: '800'
    lineHeight: 38px
    letterSpacing: -0.02em
  headline-lg:
    fontFamily: Montserrat
    fontSize: 24px
    fontWeight: '700'
    lineHeight: 30px
    letterSpacing: -0.01em
  headline-md:
    fontFamily: Montserrat
    fontSize: 18px
    fontWeight: '700'
    lineHeight: 24px
    letterSpacing: 0em
  headline-sm:
    fontFamily: Montserrat
    fontSize: 15px
    fontWeight: '600'
    lineHeight: 20px
    letterSpacing: 0em
  body-lg:
    fontFamily: Montserrat
    fontSize: 15px
    fontWeight: '400'
    lineHeight: 22px
  body-md:
    fontFamily: Montserrat
    fontSize: 13px
    fontWeight: '400'
    lineHeight: 18px
  body-sm:
    fontFamily: Montserrat
    fontSize: 11px
    fontWeight: '400'
    lineHeight: 16px
  label-lg:
    fontFamily: Space Grotesk
    fontSize: 13px
    fontWeight: '700'
    lineHeight: 16px
    letterSpacing: 0.06em
  label-md:
    fontFamily: Space Grotesk
    fontSize: 11px
    fontWeight: '600'
    lineHeight: 14px
    letterSpacing: 0.08em
  label-sm:
    fontFamily: Space Grotesk
    fontSize: 10px
    fontWeight: '500'
    lineHeight: 12px
    letterSpacing: 0.1em
rounded:
  sm: 0.125rem
  DEFAULT: 0.25rem
  md: 0.375rem
  lg: 0.5rem
  xl: 0.75rem
  full: 9999px
spacing:
  gutter: 1rem
  margin: 1.25rem
  space-xs: 0.25rem
  space-sm: 0.5rem
  space-md: 0.75rem
  space-lg: 1.25rem
  space-xl: 2rem
---

## Brand & Style

This design system combines high-contrast desktop utility with competitive gaming energy. Designed specifically for a compact, fast-booting Tauri desktop client, the interface balances raw utilitarian simplicity inspired by semantic HTML engines (like Pico.css) with the aggressive, punchy identity of competitive faction PvP.

The aesthetic fuses **Modern Dark-Mode Minimalism** with **Subtle Cyber-Tactile Accents**. Interfaces are dark, dense, and hyper-focused: deep obsidian backdrops frame hot chromatic highlights, keeping system resource overhead low while delivering instantaneous visual hierarchy. Information density is high, fluff is stripped away, and typography is geometric and commanding. The interface evokes speed, operational readiness, and high-stakes survival.

## Colors

The color palette centers on intense chromatic thermal energy emerging from an obsidian void.

- **Primary (`#FF5C00`)**: A saturated, blistering orange used for primary actions, current launcher states, active toggles, and hero interactive elements.
- **Secondary (`#FFB800`)**: A molten golden-yellow accent employed for critical alerts, XP counters, active server multipliers, and secondary telemetry badges.
- **Neutral (`#141414`)**: A grounded, noise-free charcoal black that serves as the base surface canvas.
- **Deep Obsidian Surfaces (`#0D0D0E` and `#18181A`)**: Recessed canvas bases and structural containers providing crisp contrast against pure white foreground typography.
- **Muted Structural Grays (`#8A8A93` and `#2B2B30`)**: Clean divider lines, input field resting borders, and low-priority system metadata.

The system enforces strict high-contrast rules: interactive elements must maintain clear luminance shifts on hover, avoiding soft mid-tones in favor of decisive binary states.

## Typography

The type system pairs the punchy, geometric weight of **Montserrat** with the technical, monospaced precision of **Space Grotesk**.

- **Montserrat** drives headlines, main actions, and body reading text. Rendered with tight tracking and stout weights (600–800) for headlines, it captures an assertive e-sports character. Body weights remain clean at 400 with disciplined line heights.
- **Space Grotesk** is reserved for labels, telemetry counters, build hashes, memory allocators, ping badges, and uppercase category headers. Its mechanical construction delivers crisp legibility at small sizes (10px–13px) while reinforcing the client's desktop tooling aesthetic.
- Uppercase transformations are applied systematically across buttons, tabs, server regions, and tags, accompanied by positive letter spacing for optimal readability against dark surfaces.

## Layout & Spacing

The layout model is tailored for fixed-ratio desktop application frames (optimized for standard launcher windows, e.g., 960x600 to 1280x768).

- **Structure**: Employs an asymmetrical split view: a slim, fixed utility sidebar (or top app-rail) and an expansive main staging area partitioned via an integrated CSS grid.
- **Pico-inspired Semantics**: Native HTML structures (`<main>`, `<article>`, `<header>`, `<footer>`, `<aside>`) govern global framing.
- **Rhythm**: Compact 4px-base stepping ensures tight alignment for hardware monitoring dials, profile pickers, and launch panels.
- **Desktop Window Boundaries**: Non-client drag regions sit at the top 32px bar, with internal margins locked at `1.25rem` to prevent edge crowding while maximizing internal real estate for news carousels and changelog feeds.

## Elevation & Depth

Visual hierarchy rejects heavy skeuomorphism and blur heavy backdrops in favor of **Tonal Layering with Crisp Thermal Glows**.

- **Level 0 (Canvas Base)**: Deepest background (`#0D0D0E`), matte, non-reflective.
- **Level 1 (Structural Modules & Cards)**: Container panels at `#18181A` enclosed in a hairline `1px solid #2B2B30` boundary. Zero ambient drop shadow.
- **Level 2 (Interactive Elements & Overlays)**: Flyouts, tooltips, and modal configs at `#222226` with a high-contrast perimeter: `1px solid #3E3E46` and an immediate localized drop shadow (`0 8px 24px rgba(0, 0, 0, 0.65)`).
- **Thermal Accent Glows**: Reserved strictly for the primary "PLAY" action and critical active states. Replaces heavy blurred shadows with a razor-thin luminous perimeter: `box-shadow: 0 0 16px rgba(255, 92, 0, 0.35)`.

## Shapes

The geometric framework uses a **Soft / Architectural Sharpness** (`roundedness: 1`). 

- Standard inputs, toggles, sub-modules, and chips feature subtle `0.25rem` (4px) corner rounding.
- Core containers and hero modal sheets employ `0.5rem` (8px).
- Pill shapes are completely avoided; corner radii stay taut and geometric to preserve an industrial, high-performance desktop tool aesthetic.

## Components

### Buttons
- **Hero Primary ("PLAY / LAUNCH")**: Background `#FF5C00`, text `#FFFFFF`, bold uppercase Montserrat, 4px radius. Hover: `#FF7226` with `0 0 16px rgba(255, 92, 0, 0.4)`. Active: micro-scale `0.98` and `#E05200`.
- **Secondary / Action**: Background `#222226`, border `1px solid #2B2B30`, text `#FFFFFF`. Hover: border `#FF5C00`, text `#FF5C00`.
- **Ghost / Tooling**: Transparent background, text `#8A8A93`. Hover: text `#FFFFFF`, background `#18181A`.

### Input Fields & Memory Allocator Sliders
- Text fields use native Pico-like styling: background `#0D0D0E`, border `1px solid #2B2B30`, text `#FFFFFF`, Space Grotesk font. Focus ring is a clean `1px solid #FF5C00` with no default browser outline.
- Range sliders (RAM allocation) feature a track at `#222226` with an active fill of `#FF5C00`. The thumb is a sharp 12px square or 4px rounded block in solid `#FFFFFF`.

### Cards & News Modules
- Background `#18181A`, border `1px solid #2B2B30`.
- Hover on interactive cards triggers a border shift to `#FF5C00` and subtle top translation (-2px) without layout shift.
- Headers inside cards feature uppercase Space Grotesk labels in `#FFB800` above bold Montserrat titles.

### Server Status Chips & Badges
- Pill shapes avoided; chips are compact rectangles with `0.25rem` radius.
- Background `#222226`, text in Space Grotesk 11px uppercase.
- Status indicators feature a 6px solid dot: `#00E575` (Online / Low Ping), `#FFB800` (Maintenance), or `#FF334B` (Full / Offline).

### Selectors, Checkboxes & Radio Toggles
- Custom semantic inputs. Checkboxes: 16x16px boxes, `#0D0D0E` surface with a `1px solid #2B2B30` border. Checked state fills `#FF5C00` with a stark white inner tick mark.
- Account / Faction Switcher: Compact custom dropdown showing avatar thumbnail, ign tag, and quick-auth chevron.

### Progress & Download Bars
- Background track `#141414` inset with border `1px solid #2B2B30`.
- Active fill: Solid `#FF5C00` or dynamic gradient from `#FF5C00` to `#FFB800`.
- Telemetry readout (e.g., `42.8 MB/s - 84%`) rendered below using `label-sm` Space Grotesk in `#8A8A93`.