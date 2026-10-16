#!/usr/bin/env bash
# scrub-check.sh: scan the tracked tree for anything that identifies a person,
# a machine, a network or an account.
#
#   tools/scrub-check.sh [pathspec...]
#
# Every file `git ls-files` lists (or only those under the pathspecs given) is
# read. A text file is checked line by line; a binary file (one with a NUL in
# it: images, font fixtures) is checked through its printable runs
# of four or more bytes, the way `strings` sees it, and a hit is reported by
# decimal byte offset. The exit status is 1 if anything was found, 0 if not.
#
# Two kinds of check:
#
# **Shapes**, always on, which need no private knowledge:
#
# - a MAC address (six hex pairs joined by `:`, `_` or `-`, as BlueZ object
#   paths spell them) that is not locally administered `02:…`, all zeros or
#   all ones; and the MAC inside a DHCP client id (`01:` and six pairs);
# - a global IPv6 address (`2xxx:`) outside the documentation prefix
#   2001:db8::/32;
# - an IPv4 address that is public: anything outside the private, loopback,
#   link-local, shared (100.64.0.0/10), documentation (192.0.2.0/24,
#   198.51.100.0/24, 203.0.113.0/24), benchmarking and multicast ranges;
# - an email address, except at example.com/.org/.net and the reserved
#   `.example`, `.invalid`, `.test` and `.localhost` names, `git@` remotes and
#   a bare `noreply@`;
# - a PEM private key header;
# - an absolute home directory path (`/home/<name>/`, `/var/home/<name>/`),
#   unless the name is a placeholder such as `you`, `user` or `tester`, or
#   `linuxbrew`, Homebrew's prefix on Linux, which is no one's home.
#
# **Words**, from an optional file that is never committed: one per line,
# matched case-insensitively as plain substrings (blank lines and lines
# starting `#` are skipped). Put in it whatever the shapes cannot know: the
# machine's hostname, the SSIDs and connection names of networks you use, your
# name, your email, the names of paired devices, your account names. The file
# is `$PUNCHCUTTER_SCRUB_WORDS` if set, else `~/.config/punchcutter/scrub-words`; if it does
# not exist, only the shapes are checked and the script says so.
#
#   PUNCHCUTTER_SCRUB_WORDS=~/private/scrub-words tools/scrub-check.sh
#
# What it cannot see: a value stored as raw bytes rather than text (an address
# packed into a binary fixture, or any long hex run, which has no shape to tell
# it from a hash). Fixtures are written from invented data so that this does
# not matter; the scan is the check that nothing slipped in as text.

set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

words=${PUNCHCUTTER_SCRUB_WORDS:-$HOME/.config/punchcutter/scrub-words}

git ls-files -z -- "$@" | perl -0777 -e '
use strict;
use warnings;

my ($words_file) = @ARGV;
local $/ = "\0";
my @files = split /\0/, do { local $/; <STDIN> // "" };

my @words;
if (open my $fh, "<", $words_file) {
    local $/ = "\n";
    while (my $line = <$fh>) {
        chomp $line;
        $line =~ s/\r$//;
        next if $line =~ /^\s*(#|$)/;
        $line =~ s/^\s+|\s+$//g;
        push @words, $line;
    }
    printf STDERR "scrub-check: %d word(s) from %s\n", scalar @words, $words_file;
} else {
    print STDERR "scrub-check: no words file at $words_file; checking shapes only\n";
}
my @word_res = map { qr/\Q$_\E/i } @words;

# The literal patterns are spelled so that this script does not match itself.
my $home_re = qr{(?:\/var)?\/home\/[a-z_][a-z0-9_-]*\/};
my $key_re  = qr{PRIVATE[ ]KEY-----};
# Names that are placeholders in examples, not the home of anyone.
my $placeholders = "you|user|username|tester|test|x|me|alice|bob|example";

sub ipv4_allowed {
    my @o = @_;
    my $n = ($o[0] << 24) | ($o[1] << 16) | ($o[2] << 8) | $o[3];
    my @ranges = (
        ["0.0.0.0", 8], ["10.0.0.0", 8], ["100.64.0.0", 10], ["127.0.0.0", 8],
        ["169.254.0.0", 16], ["172.16.0.0", 12], ["192.0.0.0", 24],
        ["192.0.2.0", 24], ["192.168.0.0", 16], ["198.18.0.0", 15],
        ["198.51.100.0", 24], ["203.0.113.0", 24], ["224.0.0.0", 3],
    );
    for my $r (@ranges) {
        my @b = split /\./, $r->[0];
        my $base = ($b[0] << 24) | ($b[1] << 16) | ($b[2] << 8) | $b[3];
        my $mask = (0xffffffff << (32 - $r->[1])) & 0xffffffff;
        return 1 if ($n & $mask) == $base;
    }
    return 0;
}

sub mac_allowed {
    my @g = map { lc } @_;
    return 1 if $g[0] eq "02";
    return 1 if !grep { $_ ne "00" } @g;
    return 1 if !grep { $_ ne "ff" } @g;
    return 0;
}

my $hits = 0;
sub hit {
    my ($where, $kind, $what) = @_;
    print "$where: $kind: $what\n";
    $hits++;
}

sub check {
    my ($where, $text) = @_;

    # MAC addresses: a run of hex pairs with one separator throughout.
    while ($text =~ /(?<![0-9A-Fa-f])([0-9A-Fa-f]{2}([:_-])(?:[0-9A-Fa-f]{2}\2){4,}[0-9A-Fa-f]{2})(?![0-9A-Fa-f])/g) {
        my ($run, $sep) = ($1, $2);
        my @g = split /\Q$sep\E/, $run;
        if (@g == 6) {
            hit($where, "mac", $run) unless mac_allowed(@g);
        } elsif (@g == 7 && $g[0] eq "01") {
            hit($where, "dhcp client id", $run) unless mac_allowed(@g[1 .. 6]);
        }
    }

    # Global IPv6.
    while ($text =~ /(?<![0-9A-Za-z:.])(2[0-9A-Fa-f]{3}:[0-9A-Fa-f]{0,4}:[0-9A-Fa-f:]*)/g) {
        my $addr = $1;
        # An address is compressed (`::`) or has its eight groups; a date
        # such as `2006:06:06` (EXIF writes them so) is neither.
        next unless $addr =~ /::/ || ($addr =~ tr/://) >= 7;
        hit($where, "ipv6", $addr) unless $addr =~ /^2001:0?db8:/i;
    }

    # Public IPv4.
    while ($text =~ /(?<![0-9A-Za-z.])(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})(?![0-9]|\.[0-9])/g) {
        my @o = ($1, $2, $3, $4);
        next if grep { $_ > 255 } @o;
        hit($where, "ipv4", join(".", @o)) unless ipv4_allowed(@o);
    }

    # Email addresses.
    while ($text =~ /([A-Za-z0-9._%+-]+)@((?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,})\b/g) {
        my ($local, $domain) = ($1, lc $2);
        next if $local eq "git" || $local eq "noreply";
        next if $domain =~ /(^|\.)example\.(com|org|net)$/;
        next if $domain =~ /\.(example|invalid|test|localhost)$/ || $domain =~ /^(example|invalid|test|localhost)$/;
        hit($where, "email", "$local\@$domain");
    }

    hit($where, "private key", "PEM header") if $text =~ $key_re;
    while ($text =~ /($home_re)/g) {
        my $path = $1;
        next if $path =~ m{\/home\/(?:$placeholders)\/$};
        # Homebrew on Linux installs under its own account, linuxbrew; the
        # prefix is the same on every machine and names no one.
        next if $path =~ m{\/home\/linuxbrew\/$};
        hit($where, "home path", $path);
    }

    for my $i (0 .. $#word_res) {
        hit($where, "word", $words[$i]) if $text =~ $word_res[$i];
    }
}

my $scanned = 0;
for my $file (@files) {
    next unless -f $file && !-l $file;
    open my $fh, "<:raw", $file or do { warn "scrub-check: $file: $!\n"; next };
    my $body = do { local $/; <$fh> } // "";
    close $fh;
    $scanned++;
    if (index($body, "\0") >= 0) {
        while ($body =~ /([\x20-\x7e\t]{4,})/g) {
            my $off = $-[1];
            check("$file\@$off", $1);
        }
    } else {
        my $n = 0;
        for my $line (split /\n/, $body, -1) {
            $n++;
            check("$file:$n", $line);
        }
    }
}

printf STDERR "scrub-check: %d file(s) scanned, %d hit(s)\n", $scanned, $hits;
exit($hits ? 1 : 0);
' "$words"
