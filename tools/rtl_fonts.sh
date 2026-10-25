#!/bin/sh
# Make assets/fallback-arabic.ttf and assets/fallback-hebrew.ttf: the letters of those
# scripts only, so they sit behind the UI font without taking over anything it draws.
# egui joins nothing itself (src/rtl.rs picks the joined forms), so the subsets keep the
# presentation forms and drop the shaping tables.
#
# Usage: tools/rtl_fonts.sh Vazirmatn-Regular.ttf NotoSansHebrew-Regular.ttf [pyftsubset]
#   Vazirmatn v33.003 (OFL): https://github.com/rastikerdar/vazirmatn/releases
#     vazirmatn-v33.003.zip, fonts/ttf/Vazirmatn-Regular.ttf
#   Noto Sans Hebrew v3.001 (OFL): https://github.com/notofonts/hebrew/releases
#     NotoSansHebrew-v3.001.zip, NotoSansHebrew/unhinted/ttf/NotoSansHebrew-Regular.ttf
set -eu
cd "$(dirname "$0")/.."
pyft=${3:-pyftsubset}
subset() {
    "$pyft" "$1" --unicodes="$2" --output-file="$3" --no-hinting --desubroutinize \
        --layout-features= --name-IDs=0,1,2 --drop-tables+=FFTM,GDEF,GPOS,GSUB,DSIG
}
subset "$1" "U+0600-06FF,U+0750-077F,U+0870-08FF,U+FB50-FDFF,U+FE70-FEFF" assets/fallback-arabic.ttf
subset "$2" "U+0590-05FF,U+FB1D-FB4F" assets/fallback-hebrew.ttf
ls -l assets/fallback-arabic.ttf assets/fallback-hebrew.ttf
