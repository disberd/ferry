#!/bin/sh
# Makes the five WAVs of ferry: 22.05 kHz, mono, 16-bit PCM. Needs ffmpeg.
# blocked-a and done-a come from herdr (Apache-2.0, see NOTICE):
#   HERDR_SOUNDS=<herdr checkout>/assets/sounds sounds/gen.sh
# The other three are synthesized here. Run this only to change a sound; the repo keeps the WAVs.
set -eu
: "${HERDR_SOUNDS:?set HERDR_SOUNDS to the assets/sounds folder of herdr}"
cd "$(dirname "$0")"
fmt="-ar 22050 -ac 1 -c:a pcm_s16le -map_metadata -1 -fflags +bitexact"

ffmpeg -v error -y -i "$HERDR_SOUNDS/request.mp3" $fmt blocked-a.wav
ffmpeg -v error -y -i "$HERDR_SOUNDS/done.mp3" $fmt done-a.wav

# Name, length in s, expression of t. The 5 ms fade-in prevents clicks.
syn() {
  ffmpeg -v error -y -f lavfi -i "aevalsrc='min(1,t*200)*($3)':s=22050:d=$2" $fmt "$1.wav"
}
# n(freq, start, decay): one sine note with an exponential decay.
n() { echo "gte(t,$2)*sin(2*PI*$1*(t-$2))*exp(-$3*(t-$2))"; }

syn arrival-b 0.4  "0.4*($(n 1319 0 10) + 0.3*$(n 2638 0 16))"
syn error-a   0.45 "0.4*(lt(t,0.15)*(sin(2*PI*330*t)+0.3*sin(6*PI*330*t)) + gte(t,0.18)*lt(t,0.42)*(sin(2*PI*220*t)+0.3*sin(6*PI*220*t)))"
syn clip-b    0.35 "0.4*sin(2*PI*(660*t+330*t*t/0.3))*exp(-5*t)"
