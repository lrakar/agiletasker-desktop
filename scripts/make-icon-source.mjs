#!/usr/bin/env node
/**
 * Builds the app-icon SOURCE image from the brand mark.
 *
 * Why this exists: the icons were first generated from `public/icon-512.png`,
 * which is the maskable PWA icon — it carries an OPAQUE near-black plate, so
 * the Windows taskbar/installer icon showed a black square behind the mark.
 * `public/AgileTaskerTransparent.png` is the same mark on real transparency,
 * but it runs edge-to-edge (content bbox 0..1016 of 1024), which reads as
 * cramped once the OS scales it down next to other icons.
 *
 * So: take the transparent mark, scale it to ~78% and centre it on a 1024
 * transparent canvas. That inset is roughly what platform icon guidelines
 * assume for a free-standing glyph, and it keeps the mark from touching the
 * dock/taskbar edges.
 *
 * Output: desktop/src-tauri/icon-source.png — the input for `tauri icon`.
 * Regenerate the icon set with:
 *   node scripts/make-icon-source.mjs && npx tauri icon src-tauri/icon-source.png
 */
import { createCanvas, loadImage } from '@napi-rs/canvas'
import { writeFileSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const HERE = path.dirname(fileURLToPath(import.meta.url))
const REPO = path.resolve(HERE, '..', '..')
const SRC = path.join(REPO, 'public', 'AgileTaskerTransparent.png')
const OUT = path.join(HERE, '..', 'src-tauri', 'icon-source.png')

const SIZE = 1024
const SCALE = 0.78

const img = await loadImage(SRC)
const canvas = createCanvas(SIZE, SIZE)
const ctx = canvas.getContext('2d')
const target = SIZE * SCALE
const ratio = Math.min(target / img.width, target / img.height)
const w = img.width * ratio
const h = img.height * ratio
ctx.drawImage(img, (SIZE - w) / 2, (SIZE - h) / 2, w, h)

writeFileSync(OUT, canvas.toBuffer('image/png'))
console.log(`icon source written: ${OUT} (${SIZE}x${SIZE}, mark at ${Math.round(SCALE * 100)}%)`)
