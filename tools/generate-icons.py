#!/usr/bin/env python3
"""Rebuild the bundled Phosphor icon atlas (development only).

Interface glyphs use Phosphor's bold weight; brand logos, badges and file types keep the fill.

Downloads the version-pinned SVG sources, verifies every file hash, composes one SVG grid and
rasterizes it with the `resvg` command-line tool. Requires `resvg` 0.45.1 on PATH or `--resvg`:

    cargo install resvg --version 0.45.1 --root /tmp/resvg-tool
    python3 tools/generate-icons.py --resvg /tmp/resvg-tool/bin/resvg
"""

import argparse
import hashlib
from pathlib import Path
import shutil
import subprocess
import sys
import urllib.request

VERSION = "2.1.1"  # @phosphor-icons/core on npm, MIT
BASE = f"https://cdn.jsdelivr.net/npm/@phosphor-icons/core@{VERSION}"
COLUMNS, CELL, PAD = 8, 64, 4
GLYPH = CELL - 2 * PAD

# Repository-local artwork, rasterized from the repository instead of an upstream package.
# `assets/icons/thread.svg`, `hang-up.svg`, `noise.svg` and `window.svg` are listed inline below.
BRAND_MARK = "repo:assets/brand/leech-mark.svg"

# name, upstream asset, SHA-256 of the unmodified SVG file.
ICONS = [
    ("caret-down", "bold/caret-down-bold.svg", "76a97545e1b923bc13bcc15d7bcbb7f5530105e6eaa98a18c1e30d23e3622843"),
    ("caret-right", "bold/caret-right-bold.svg", "03cadd956d715541432ec8dc2eda1c53ca341af7d3ccecb8dd32c5b9747e290f"),
    ("gear", "bold/gear-six-bold.svg", "78046b12e51bcbb83a744bf49cb2b6f6830e102374a58d9e08702cb995f9f95f"),
    ("microphone", "bold/microphone-bold.svg", "129f28eb3ba66fd180d860b3e46f0e794af24700d05e7cc025aac560a9469f64"),
    ("microphone-slash", "bold/microphone-slash-bold.svg", "6a5eda3a51e3d2abfa198f6487248ad8dd54122fc0cb700cc53e4469322abb41"),
    ("headphones", "bold/headphones-bold.svg", "163a8eef42bf409f7a4670cc22e3c503d34e18a67dd779a6d25257ec840246d1"),
    ("headphones-slash", "bold/headphones-bold.svg", "163a8eef42bf409f7a4670cc22e3c503d34e18a67dd779a6d25257ec840246d1"),
    ("push-pin", "bold/push-pin-bold.svg", "47cf424c346b4cf5b581f037499d519796735f4c1b31a0ec44b945d43779aa4b"),
    ("users", "bold/users-bold.svg", "3483c79bd35afe632197363741da6018b1204b168cf3f34e7073f574306677a8"),
    ("user-plus", "bold/user-plus-bold.svg", "9a26eedd8f06b1ad21bc20fafe592e6a3de98af814f1c4aa6551a819ef0b4638"),
    ("user-circle", "bold/user-circle-bold.svg", "fb8674723397155e46003ed181259e4400c12512b8f0285e8d163e536088f96c"),
    ("magnifying-glass", "bold/magnifying-glass-bold.svg", "b73e393b20bff0aaee96b9e325d0276fe1ab5fc81b5080633a95827bd14ebae6"),
    ("plus", "bold/plus-bold.svg", "3d20a4b2e00657baeb922bed94f13fbfa288968b738991d047dd252cb64005d8"),
    ("plus-circle", "bold/plus-circle-bold.svg", "ea66e21d33855475406aad71a2ea1d16b79b467ac1753d2788cb84c76fc6cc11"),
    ("x", "bold/x-bold.svg", "d540487912a267d83c495954b24ca07981002fda05ee2ea0b492d8fc188d1c3e"),
    ("smiley", "bold/smiley-bold.svg", "4a0c410bbee53d46da10a79bf0d5d0b57a6694bdbfe169ff4c25101734444520"),
    ("bell", "bold/bell-bold.svg", "5e63a3fb68acd80f4fe9c7478c2c9f8b175999b4da6393a45195211e6504fd1b"),
    ("phone", "bold/phone-bold.svg", "61826bd13e5100b0079815888c38338ef4586d0febb96ad0e8c9420f5ea27a60"),
    ("phone-call", "bold/phone-call-bold.svg", "4df36a70b3cd7e5d4e9e33433fbd2841e3bbd465cd73e2193c865b86a9c1f02e"),
    ("hang-up", "repo:assets/icons/hang-up.svg", "a89e70c3db9afe4dfc7ce090ec12312f754add0f65190eedd333650ed337c397"),
    ("video-camera", "bold/video-camera-bold.svg", "1c9ce26c2682f693603519c54392924581cd19393e52792d27ceab51ca8f9eb2"),
    ("video-camera-slash", "bold/video-camera-slash-bold.svg", "01121a8380ed14b351b87eaa3bb3b0d9271afbfb08c2261b7194258e812d629b"),
    ("monitor-arrow-up", "bold/monitor-arrow-up-bold.svg", "16082af7c44d5e2c67306cc2eec360cc1a6093261e2c45c6f95d375ab2bc39ec"),
    ("rocket-launch", "bold/rocket-launch-bold.svg", "17cdf50ac375807224f127d664edd1b1090dad01a4156c6d9d042f49beb0475d"),
    ("waveform", "bold/waveform-bold.svg", "d9f45452d0caf9ef321d3b506dd56442216c042f3d8daab9ba760e2e94c7b09e"),
    ("arrow-bend-up-left", "bold/arrow-bend-up-left-bold.svg", "b59c6a1dea610f669a69920bece2138108a54d88eaa9a1d0a20f82e1657c9f25"),
    ("pencil-simple", "bold/pencil-simple-bold.svg", "163a4e787655de6b5b50bfb0dc0e6818c015ba32106eb14fe12c3daf033a892b"),
    ("dots-three", "bold/dots-three-bold.svg", "5bd06c9754d075b409e3259350c9abc9ec106ffb3b6f3cd451f6419271163bc1"),
    ("tray", "bold/tray-bold.svg", "614b4a9cce26501a6a12500dc8f68299cb738061f75b316b910544ebfa5093f2"),
    ("question", "bold/question-bold.svg", "605254a6f6dc50f1a34bf7d8995d01902e064c2e459fbe8108b8f7890de7c259"),
    ("arrow-clockwise", "bold/arrow-clockwise-bold.svg", "773d814cdf564a4d0db6e0408a60e5bd97c76c90293aaf383c72399ce2e034a1"),
    ("chats", "bold/chats-bold.svg", "d28c08b774a903da4244d95624966c4f630b7ad072ba2a7eabe1d91187438dfe"),
    ("speaker-high", "bold/speaker-high-bold.svg", "f0fc825cb96958238882b98aea55afdb63da92c4c5c8b499c0e1068e9abb299e"),
    ("hash", "bold/hash-bold.svg", "8d2cb5e39903004da3d0a85a919adc6c76adcd65b3c9c1e8e355d8cf6cdf7193"),
    ("chat-centered-text", "bold/chat-centered-text-bold.svg", "401aa39ceaa3622ca1a6cf79393996ac00c21089aeaa188c9570691ee0e7d4d5"),
    ("paper-plane-right", "bold/paper-plane-right-bold.svg", "6952ad0ffc0f565a00231f797891d001e0e36b68255cb402b13bccf881f8d27f"),
    ("arrow-square-out", "bold/arrow-square-out-bold.svg", "68348bacc7f539b8d73de92d7c120f22e424b4364374ccab01719ada8c8a12aa"),
    ("github-logo", "fill/github-logo-fill.svg", "35b2036490005bcb95a68d07d43d441ae585c6c6dde0e902bb3daf0ddbc615b9"),
    ("twitch-logo", "fill/twitch-logo-fill.svg", "df5adaa427994af42c961715d6e75f496b1832606a68cbd8090d09c25a4e8361"),
    ("steam-logo", "fill/steam-logo-fill.svg", "1715fdfe47e32c6cbff76fd80dcb47c1e5a704b2067412dd64dff20b8d802a11"),
    ("spotify-logo", "fill/spotify-logo-fill.svg", "50360bee2998f48367780b6f1637d208169acb0d3384e27e145f29b456b73bd4"),
    ("youtube-logo", "fill/youtube-logo-fill.svg", "fce097e65d0df0cb2bb84cd282bb6cc08ccb47ec18d41b7c22870830257065be"),
    ("x-logo", "fill/x-logo-fill.svg", "b760dcebe932bafc69784799231eb8ce988dc33fa5d68d20e706508b24264526"),
    ("reddit-logo", "fill/reddit-logo-fill.svg", "f6e1c29f724ad06cf8ab8c41e26a72a482e6187724ac7cc91fe880b9c499873d"),
    ("facebook-logo", "fill/facebook-logo-fill.svg", "4ef00877d0348c4db3eb93d01d36d9b82528ca2d635d0e3a5e51515299cf99f7"),
    ("instagram-logo", "fill/instagram-logo-fill.svg", "24444c671b0c50580b39138453762225d4d53b7d04c978244b8027a84c719746"),
    ("tiktok-logo", "fill/tiktok-logo-fill.svg", "aafb59c3c1902035736bc955e1b4485e46fd8b633bb2cd2e046937cc1b26c5cb"),
    ("paypal-logo", "fill/paypal-logo-fill.svg", "b49cb17698ca5c2d807577413f992b9d0050b2a94ca4c812c6e6441a32f9f820"),
    ("amazon-logo", "fill/amazon-logo-fill.svg", "96d475f1eaed1733e9c6dcae74b52900be7e399b128917a8f50d89d773041491"),
    ("mastodon-logo", "fill/mastodon-logo-fill.svg", "f9d1bf489b16a34763aa7f8e8de62f72812cb9888fd92bbc47eeac33d1426122"),
    ("skype-logo", "fill/skype-logo-fill.svg", "bd264055d7317fb0cfe3a39ae17028d2a5447dc3a204a999c5e5fbebed423e28"),
    ("game-controller", "bold/game-controller-bold.svg", "0e0edfffa8a5154561407ff3069e2f5bc4e8422db13d429923c0a611b2f142d5"),
    ("television", "bold/television-bold.svg", "4d7e50cf5736b205c2989218c3e17735faeb84800f1a00b111797dce75ce3828"),
    ("globe", "bold/globe-bold.svg", "b1dff2302e56cfc39cb704bb109486ee53d29a9a65f5a02f50e258259fbf743e"),
    ("link", "bold/link-bold.svg", "087d63d78d4a37d570853cb62e688323759cda2081b22455674544f87adc3444"),
    ("copy", "bold/copy-bold.svg", "204e84365593c418c71d9ce0674be3dd3e5ff13d0dc13e1f728249ef9571bf9e"),
    ("seal-check", "fill/seal-check-fill.svg", "27702a62622ac4156d2a18b10fc7c526b3b2b9f782741e2cb07aea6964d71132"),
    ("calendar-blank", "bold/calendar-blank-bold.svg", "4a87a0b65ed1912ec706f9ce2bd35d77bca373454c7f944494e89abf99dcae89"),
    ("leech-mark", BRAND_MARK, "9f410cec397191090cf9ce3eab623672ad33ba22a5aa70cc014a1e595159ea71"),
    ("file", "fill/file-fill.svg", "d6fe00691e45b5e9b87ccc5a8fc9022485935408167ab1234817915df4fd1ca4"),
    ("file-image", "fill/file-image-fill.svg", "0266eb983ed5cec9152d76691152123b92b2cb84d8c139c37793d0870df3f601"),
    ("file-pdf", "fill/file-pdf-fill.svg", "12622b293b9a1efa1f56969ca74ab88085d388c4be926f81cfcb49406a8da05d"),
    ("file-zip", "fill/file-zip-fill.svg", "3b4355954bef19b2572639dcb619e0e81513118dd76c3c2fc8bab6d9a23c9032"),
    ("file-text", "fill/file-text-fill.svg", "8c6f36452a441d39886ad7929778c2a90ea66cca519490da4918e7da7bade242"),
    ("file-code", "fill/file-code-fill.svg", "127044e7395681d5306c7553cc2287e16c63f9d4578f4f6fe7710f277e5b7624"),
    ("file-audio", "fill/file-audio-fill.svg", "835c6bfde42e68d716db3dd4db0e02843f6a7bd704a4377287a4b022d80ace7a"),
    ("file-video", "fill/file-video-fill.svg", "2940d83bf3560f389d6a826ca639741a9098d5bfe8910ea371fdc4c83790b827"),
    ("trash", "bold/trash-bold.svg", "14f3fa7bf3588b279a104e6a19b16809fa54f1c2185e2811d3c47cab559f79d5"),
    ("arrow-down", "bold/arrow-down-bold.svg", "193222b87a796f1c56336f51cb041cfd2a6c899ae0a027248a63fc183758a364"),
    ("arrow-up", "bold/arrow-up-bold.svg", "87dade5b87b48190dae13370375e2d84f6c22548eebac93ff2f46d091882fb78"),
    ("check", "bold/check-bold.svg", "d0ca4e324ff5bb3a1a3bacb9f7580359b8e03cc6862a614d5ed14458db64bedf"),
    ("gif", "bold/gif-bold.svg", "cbdd66cdfbe9f084d2ecc7e7a27686830b5230cc0a18f9765fb9c147ac8e0647"),
    ("star", "bold/star-bold.svg", "e456b195ce0f28235d63c0612b835fd120c747a8132d2be984863e2125674240"),
    ("star-fill", "fill/star-fill.svg", "42451b34121b695bdaab88fdcf3eacec9cda9ac36d222349609426dcb9f04b48"),
    ("fire", "fill/fire-fill.svg", "512355fb2a156f0c39485a28bc6f81cf1df3de35faed9c5ad62c30cec5d8d63c"),
    ("arrow-left", "bold/arrow-left-bold.svg", "7588792d7824e7c5337bca7b5de96ad0685ddea0a5732dfb62701d163a076563"),
    ("folder", "bold/folder-bold.svg", "3f564dd4a0d27706ff9cb2d9738cef9ac1009b70f82d1da6d3bac119e41949a0"),
    ("folder-open", "bold/folder-open-bold.svg", "a7eb8f25dde176db06b901a0b06b36f1723c24651e5c28e4fa0403f2a7624aba"),
    ("caret-left", "bold/caret-left-bold.svg", "b78b6f532b53b9847340961848cb9e4f5be7e1da07e9dcf11c60cf64aa3986b7"),
    ("download-simple", "bold/download-simple-bold.svg", "3ca0b8bba15633eb29f9818fe3670d6438bcbc897649c767e491a411e696e2d3"),
    ("arrow-right", "bold/arrow-right-bold.svg", "aa0f883acecf63d9140ad7cd51910fa4b723671c400772a3c621c9cc16b1687b"),
    ("image", "bold/image-bold.svg", "d99ef65cc79a3db6a0b213c878238e06689ef701285f207629b49868808b852e"),
    ("sparkle", "bold/sparkle-bold.svg", "d61e5264b0866af931617635cbda2cd23f7b76d2d0f6daad1560dff8f3cf4980"),
    ("compass", "bold/compass-bold.svg", "b01b0f49ade539c501b7a1b0b3d421aee58d9f46c1e1dbdfe5f25a7a4fc8b7f2"),
    ("megaphone-simple", "bold/megaphone-simple-bold.svg", "a5484baa15725b62620e590073e710bbfe2f8b1bc9f616b6cfbbf5df1ae8bef2"),
    ("shield-warning", "bold/shield-warning-bold.svg", "4f7d55d31c62860f75439545b402641ca3c9a55703f2aab0f97f7c017b51c2eb"),
    ("crown", "fill/crown-fill.svg", "a285272ce54401826e1458e5437aaa5dd78b5bb92b75092093f091eb024b0a45"),
    ("chart-bar", "bold/chart-bar-bold.svg", "dec4c21084915b1115d14bce389b66c2c6af19abdb1b61d8e752b74ecd91a404"),
    ("shopping-cart-simple", "bold/shopping-cart-simple-bold.svg", "c79f8dc13b64e7da5a40ee78f24a3aeaa150d2304937b763dc80ea3ae1a96a0d"),
    ("lock-simple", "bold/lock-simple-bold.svg", "656f6bff423aa14e5d58c60f25bec0ffd853c3fbec9514e36576fa48b545370c"),
    ("eye-slash", "bold/eye-slash-bold.svg", "2c90457e608c86c1e880b897f2258eeea8ea7f60e7eaf61592b9674d7978647e"),
    ("sliders-horizontal", "bold/sliders-horizontal-bold.svg", "e409a5fb3c2c134e46d51e48ac395e392223e04535c5ec664253f3e7e78cd7a9"),
    ("arrows-down-up", "bold/arrows-down-up-bold.svg", "174464c54af7273e46a6fc204ebd0fc1da75906573880687836b314e3fbdb85e"),
    ("thread", "repo:assets/icons/thread.svg", "dfd7daf80375504a5af37b95bee1773af55a9eabe7c802e7f4152905f123c72c"),
    ("device-mobile", "bold/device-mobile-bold.svg", "77a4a5ebcba16e37637e700381bc3858b92c9350730ecf1e207f5f40d524de03"),
    ("hard-drives", "bold/hard-drives-bold.svg", "2ba693bda78a09d3cab785bfd08e819a2907915542b5753854cab88cc728d18b"),
    ("corners-out", "bold/corners-out-bold.svg", "df74ba3c2a496a344f98e80c0782f43aa3760ed4b8b6103a4c621bfc59575e16"),
    ("noise", "repo:assets/icons/noise.svg", "739df297725815e76f63df9e773eff82b5b0b1ff0c1d7709e74c64c94ae50ccf"),
    ("noise-slash", "repo:assets/icons/noise.svg", "739df297725815e76f63df9e773eff82b5b0b1ff0c1d7709e74c64c94ae50ccf"),
    ("window", "repo:assets/icons/window.svg", "65e22a650723dba7bf87c29b6b4864314a67c284a9d5fe00225d89d71b77eec4"),
    ("user", "bold/user-bold.svg", "a4739174a602675d627b029aa524dd9345c56838ed1da34b5e1c3d02611792d5"),
    ("identification-card", "bold/identification-card-bold.svg", "48da4bc28ef11aa0b5ce3c0e95770d0ba84591e7b839be177061336b6bf4eec8"),
    ("shield-check", "bold/shield-check-bold.svg", "da3d313e3cec48b0cf45865d791f4838b5841df2ef564467d02b15ff20f507f3"),
    ("database", "bold/database-bold.svg", "f0b769c621d9263dbea9122d65188224be8474f6b124dac06a485eec83dd2bce"),
    ("palette", "bold/palette-bold.svg", "e178c6c05ec0abd053cc2d6e2eafa0a67ccee9fdafba5c6a598dd54ce5c18ab8"),
    ("chat-circle-text", "bold/chat-circle-text-bold.svg", "259305b66c9ff6a57b4fb7290cc7ad0be20b379967518603847f43a42384ca4f"),
    ("keyboard", "bold/keyboard-bold.svg", "54b30973b62402f0e56e9ae41bd8ba08d771e643dbc4699de5f858b801e7bd4d"),
    ("arrow-circle-up", "bold/arrow-circle-up-bold.svg", "fdc08d9a9a17f242f9077d55e237c31cb91c154911db3eb9a77afbe8b17dcda9"),
    ("swatches", "bold/swatches-bold.svg", "9b1d4ba49657b97b8c89dd8c9fd68ba209eff4517cdcb258d89ef0abe5a5e12c"),
    ("puzzle-piece", "bold/puzzle-piece-bold.svg", "91ecba3ea1531d9d5074723ea66ed1c15116902012103068af4f3918c2f2529a"),
    ("sign-out", "bold/sign-out-bold.svg", "7e7a96557804b578ec40ce2365f4bcea7c8fdec4f49068724d27fc03272d0156"),
    ("bell-slash", "bold/bell-slash-bold.svg", "42a3eb481b1189fddc56f95c48e406414616f8cb7084ec867e1f4fbd9d2056f2"),
    ("envelope-simple", "bold/envelope-simple-bold.svg", "624396d3d14151b70e1a46a946ad20eb35dc16968de43f183574f4d684ffa667"),
    ("prohibit", "bold/prohibit-bold.svg", "fba9bed853d84c9fc94860cb3b1504a2077ffa2da2cb99cc9e7e943636097285"),
    ("user-minus", "bold/user-minus-bold.svg", "71d0b0eaa82fe0c2cea086ae420e22d263e5102ff364ea2111d438e47ba0ee55"),
    ("at", "bold/at-bold.svg", "1c1ede3364fe072f606d30f3ffd098b6e40c56e36ac4161961d283b68eb1c8c2"),
]
LICENSE_SHA256 = "ddbe6082ec3cf979db47e5af549d2849c5d6182b3e005ef91ce1dbb9eb122f11"

# Brand marks Phosphor does not ship come from Simple Icons (CC0 1.0). Their 24-unit glyphs
# fill the whole view box, so they are drawn at Phosphor's visual size inside the cell.
SIMPLE_VERSION = "16.30.0"  # simple-icons on npm, CC0-1.0
SIMPLE_BASE = f"https://cdn.jsdelivr.net/npm/simple-icons@{SIMPLE_VERSION}"
SIMPLE_ICONS = [
    ("playstation", "icons/playstation.svg", "b68b4d7b63443759b9c4d77a5501c5758eeae06a6d594d278d5eb6cb4d3dbbe4"),
    ("battle-net", "icons/battledotnet.svg", "78206c9c5e7fd24803cd50cd5c71c3bb1b02def42ca4f5d186e318eb9e247b7e"),
    ("epic-games", "icons/epicgames.svg", "a19b1eb5a46edc11a7dc7f1ce6fa1701ea4e4cf451feec88441c523f4b50cde3"),
    ("league-of-legends", "icons/leagueoflegends.svg", "b653d9c5733c71613fa0367c09c9d419df2f0e7ef91cbeab6b93a2e20d5f223b"),
    ("riot-games", "icons/riotgames.svg", "b80c5880b88b8e489b2da7753bed612b3ce6b2948a3f995ad5d31050f443b520"),
    ("bungie", "icons/bungie.svg", "92c3b473805eaffdc7092cb8e8c1500ccac1bea19c129d46b026c22af6000db7"),
    ("roblox", "icons/roblox.svg", "9245b2f23fde91a5ef34f36aea32c1bf08a1a1254c08b86e7f060a4f72986234"),
    ("crunchyroll", "icons/crunchyroll.svg", "3b9c3d87339e18ec09f25e0c3eaffdc5ad4630df3d108fcd907e1b64c4cd13ea"),
    ("ebay", "icons/ebay.svg", "846e8d8ac6cea49766e7c62e095e739d115ed355f9b63bf46067f14c9343c745"),
    ("bluesky", "icons/bluesky.svg", "49752973164fbbf4464fbb4776f011c1eff207e5d7ad9254e031af025814eb75"),
]
SIMPLE_LICENSE_SHA256 = "9046848b63a5c92bff14e4accca80bd987e0623b74adf9226ce5198d312b79d5"
SIMPLE_SCALE = 0.8  # Phosphor fill glyphs span roughly 205 of their 256 units.


def fetch(path, base=BASE):
    with urllib.request.urlopen(f"{base}/{path}", timeout=60) as response:
        return response.read(1024 * 1024)


def inner_svg(svg):
    start = svg.index(">", svg.index("<svg")) + 1
    end = svg.rindex("</svg>")
    return svg[start:end]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--resvg", default="resvg", help="path to the resvg executable")
    parser.add_argument("--print-hashes", action="store_true", help="print SHA-256 values instead of checking")
    args = parser.parse_args()
    destination = Path(__file__).resolve().parents[1] / "assets" / "icons"
    destination.mkdir(parents=True, exist_ok=True)

    license_text = fetch("LICENSE")
    if args.print_hashes:
        print("LICENSE", hashlib.sha256(license_text).hexdigest())
    elif hashlib.sha256(license_text).hexdigest() != LICENSE_SHA256:
        raise ValueError("LICENSE SHA-256 mismatch")

    simple_license = fetch("LICENSE.md", SIMPLE_BASE)
    if args.print_hashes:
        print("SIMPLE LICENSE", hashlib.sha256(simple_license).hexdigest())
    elif hashlib.sha256(simple_license).hexdigest() != SIMPLE_LICENSE_SHA256:
        raise ValueError("Simple Icons LICENSE SHA-256 mismatch")

    sources = [(name, asset, sha256, False) for name, asset, sha256 in ICONS]
    sources += [(name, asset, sha256, True) for name, asset, sha256 in SIMPLE_ICONS]
    names = [name for name, _, _, _ in sources]
    assert len(set(names)) == len(names)
    rows = (len(sources) + COLUMNS - 1) // COLUMNS
    width, height = COLUMNS * CELL, rows * CELL
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">']
    parts.append(
        '<defs><mask id="slash" maskUnits="userSpaceOnUse" x="0" y="0" width="256" height="256">'
        '<rect width="256" height="256" fill="#fff"/>'
        '<path d="M40 24 L232 216" stroke="#000" stroke-width="44" stroke-linecap="round"/></mask></defs>'
    )
    index = []
    root = Path(__file__).resolve().parents[1]
    for cell, (name, asset, sha256, simple) in enumerate(sources):
        local = asset.startswith("repo:")
        if local:
            # Git may check text out with CRLF; the pins are taken from the LF form.
            svg = (root / asset.removeprefix("repo:")).read_bytes().replace(b"\r\n", b"\n")
        else:
            svg = fetch(asset, SIMPLE_BASE) if simple else fetch(f"assets/{asset}")
        digest = hashlib.sha256(svg).hexdigest()
        if args.print_hashes:
            print(name, asset, digest)
        elif digest != sha256:
            raise ValueError(f"{asset} SHA-256 mismatch: {digest}")
        text = svg.decode("utf-8")
        body = inner_svg(text)
        x = (cell % COLUMNS) * CELL + PAD
        y = (cell // COLUMNS) * CELL + PAD
        if local:
            # Our own mark is trimmed to its bounding box; honour its view box offset.
            box = text.split('viewBox="', 1)[1].split('"', 1)[0].split()
            left, top, size = float(box[0]), float(box[1]), float(box[2])
            scale = GLYPH / size
            x, y = x - left * scale, y - top * scale
        elif simple:
            assert 'viewBox="0 0 24 24"' in text, asset
            inset = GLYPH * (1 - SIMPLE_SCALE) / 2
            x, y, scale = x + inset, y + inset, GLYPH * SIMPLE_SCALE / 24
        else:
            assert 'viewBox="0 0 256 256"' in text, asset
            scale = GLYPH / 256
        if name.endswith("-slash") and "slash" not in asset:
            # Phosphor has no slashed headphones; compose the upstream glyph with a knocked-out
            # diagonal in the style of its own `*-slash` icons.
            body = (
                f'<g mask="url(#slash)">{body}</g>'
                '<path d="M40 24 L232 216" stroke="#fff" stroke-width="24" stroke-linecap="round"/>'
            )
        parts.append(f'<g transform="translate({x:.6f} {y:.6f}) scale({scale:.6f})" fill="#fff">{body}</g>')
        index.append((name, cell))
    parts.append("</svg>")
    if args.print_hashes:
        return
    atlas_svg = destination / "atlas.svg"
    atlas_svg.write_text("".join(parts), encoding="utf-8")
    subprocess.run([args.resvg, str(atlas_svg), str(destination / "atlas.png")], check=True)
    atlas_svg.unlink()
    # Lossless; resvg writes an unoptimized PNG about 4x larger. Install with `cargo install oxipng`.
    if shutil.which("oxipng"):
        subprocess.run(["oxipng", "-o", "max", "--strip", "all", "-q", destination / "atlas.png"], check=True)
    else:
        # White at every alpha fits one 256-entry palette; Pillow packs that close to oxipng.
        from PIL import Image

        atlas = Image.open(destination / "atlas.png").convert("RGBA")
        indexed = Image.frombytes("P", atlas.size, atlas.getchannel("A").tobytes())
        indexed.putpalette([255] * 768)
        indexed.save(destination / "atlas.png", transparency=bytes(range(256)), optimize=True)
    (destination / "index.tsv").write_text("".join(f"{name}\t{cell}\n" for name, cell in index), encoding="utf-8")
    (destination / "LICENSE").write_bytes(license_text)
    (destination / "LICENSE-SIMPLE-ICONS").write_bytes(simple_license)
    print(f"{len(sources)} icons; {width}x{height}; atlas {(destination / 'atlas.png').stat().st_size} bytes")
    for file in ["atlas.png", "index.tsv", "LICENSE", "LICENSE-SIMPLE-ICONS"]:
        print(file, hashlib.sha256((destination / file).read_bytes()).hexdigest())


if __name__ == "__main__":
    main()
