#!/usr/bin/env node

import { execSync } from 'child_process';
import { readFileSync } from 'fs';
import { resolve } from 'path';
import { cwd } from 'process';

const REPO_ROOT = cwd();
const CARGO_TOML = resolve(REPO_ROOT, 'tabt-app/Cargo.toml');
const INFO_PLIST = resolve(REPO_ROOT, 'bundle/Info.plist.in');

/**
 * Execute command and return output
 */
function run(cmd, opts = {}) {
  try {
    return execSync(cmd, {
      cwd: REPO_ROOT,
      encoding: 'utf-8',
      stdio: opts.stdio || 'pipe',
      ...opts,
    }).trim();
  } catch (e) {
    if (opts.ignoreError) return '';
    throw new Error(`Command failed: ${cmd}\n${e.message}`);
  }
}

/**
 * Read version from Cargo.toml
 */
function readVersion() {
  const content = readFileSync(CARGO_TOML, 'utf-8');
  const match = content.match(/version\s*=\s*"([^"]+)"/);
  if (!match) throw new Error('Could not find version in Cargo.toml');
  return match[1];
}

/**
 * Get last tag
 */
function getLastTag() {
  return run('git describe --tags --abbrev=0 2>/dev/null || echo ""', {
    ignoreError: true,
  });
}

/**
 * Generate changelog from commits
 */
function generateChangelog(fromTag) {
  let cmd = 'git log --oneline';
  if (fromTag) {
    cmd += ` ${fromTag}..HEAD`;
  } else {
    cmd += ' --all';
  }

  const commits = run(cmd)
    .split('\n')
    .filter(line => line.trim())
    .slice(0, 20); // Last 20 commits

  return commits;
}

/**
 * Format release notes
 */
function formatReleaseNotes(version, commits, customNotes) {
  if (customNotes) {
    return customNotes;
  }

  // Group commits by type
  const features = commits.filter(c => c.includes('Add') || c.includes('New'));
  const fixes = commits.filter(c => c.includes('Fix') || c.includes('fix'));
  const improvements = commits.filter(c =>
    c.includes('Improve') || c.includes('Compress') || c.includes('Simplify')
  );
  const other = commits.filter(c =>
    !features.includes(c) && !fixes.includes(c) && !improvements.includes(c)
  );

  let notes = `## What's new\n\n`;

  if (features.length > 0) {
    notes += `**New features**\n${features.map(c => `- ${c}`).join('\n')}\n\n`;
  }

  if (improvements.length > 0) {
    notes += `**Improvements**\n${improvements.map(c => `- ${c}`).join('\n')}\n\n`;
  }

  if (fixes.length > 0) {
    notes += `**Bug fixes**\n${fixes.map(c => `- ${c}`).join('\n')}\n\n`;
  }

  if (other.length > 0) {
    notes += `**Other changes**\n${other.map(c => `- ${c}`).join('\n')}\n\n`;
  }

  notes += `## Technical\n\nFull changelog: use \`git log v${version}...HEAD\` to see all commits since last release.`;

  return notes;
}

/**
 * Create a release
 */
function createRelease(args) {
  const version = args['--version'] || readVersion();
  const customNotes = args['--notes'];
  const tag = `v${version}`;
  const lastTag = getLastTag();

  console.log(`📦 Creating release ${tag}`);
  console.log(`   Last tag: ${lastTag || 'none (first release)'}`);

  // Check git status
  const status = run('git status --porcelain');
  if (status) {
    console.log(`\n⚠️  Uncommitted changes detected:`);
    console.log(status);
    throw new Error(
      'Please commit or stash changes before releasing. ' +
      'Run: git status'
    );
  }

  // Check if tag exists
  try {
    run(`git rev-parse ${tag}`);
    throw new Error(`Tag ${tag} already exists`);
  } catch (e) {
    if (!e.message.includes('already exists')) {
      // Tag doesn't exist, which is good
    } else {
      throw e;
    }
  }

  // Generate changelog
  const commits = generateChangelog(lastTag);
  console.log(`\n📋 Found ${commits.length} commits since last release`);

  // Format notes
  const notes = formatReleaseNotes(version, commits, customNotes);

  // Create tag
  console.log(`\n🏷️  Creating tag ${tag}...`);
  run(`git tag ${tag}`);

  // Push tag
  console.log(`📤 Pushing to origin...`);
  run(`git push origin ${tag}`);

  // Create GitHub release
  console.log(`🚀 Creating GitHub release...`);
  try {
    run(`gh release create ${tag} --title "${tag}" --notes "${notes.replace(/"/g, '\\"')}"`, {
      stdio: 'inherit',
    });
  } catch (e) {
    console.log(`⚠️  GitHub release creation failed: ${e.message}`);
    console.log(`   You may need to create it manually at:`);
    console.log(`   https://github.com/hex2null/TabT/releases/new?tag=${tag}`);
    throw e;
  }

  console.log(`\n✅ Release ${tag} created successfully!`);
  console.log(`   https://github.com/hex2null/TabT/releases/tag/${tag}`);
}

/**
 * Show what would be released
 */
function preview(args) {
  const version = args['--version'] || readVersion();
  const tag = `v${version}`;
  const lastTag = getLastTag();

  console.log(`\n📋 Release Preview: ${tag}`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`\nCurrent version: ${version}`);
  console.log(`Last tag: ${lastTag || 'none'}\n`);

  const commits = generateChangelog(lastTag);
  console.log(`Commits since last release (${commits.length} total):\n`);
  commits.forEach(commit => console.log(`  ${commit}`));

  const notes = formatReleaseNotes(version, commits);
  console.log(`\n\nRelease notes:\n`);
  console.log(notes);
}

/**
 * Show version info
 */
function info() {
  const version = readVersion();
  const lastTag = getLastTag();
  console.log(`\nℹ️  Version Info`);
  console.log(`━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━`);
  console.log(`Current version: ${version}`);
  console.log(`Last release tag: ${lastTag || 'none'}`);
  console.log(`\nTo create a release, run:`);
  console.log(`  node .claude/skills/release/driver.mjs create`);
}

/**
 * Main entry point
 */
function main() {
  const args = process.argv.slice(2);
  const command = args[0] || 'info';

  // Parse named arguments
  const parsed = { _: [] };
  for (let i = 1; i < args.length; i++) {
    if (args[i].startsWith('--')) {
      parsed[args[i]] = args[i + 1];
      i++;
    } else {
      parsed._.push(args[i]);
    }
  }

  try {
    switch (command) {
      case 'create':
        createRelease(parsed);
        break;
      case 'preview':
        preview(parsed);
        break;
      case 'info':
        info();
        break;
      default:
        console.error(`Unknown command: ${command}`);
        console.error(`\nUsage:`);
        console.error(`  driver.mjs create [--version VERSION] [--notes NOTES]`);
        console.error(`  driver.mjs preview [--version VERSION]`);
        console.error(`  driver.mjs info`);
        process.exit(1);
    }
  } catch (e) {
    console.error(`\n❌ Error: ${e.message}`);
    process.exit(1);
  }
}

main();
