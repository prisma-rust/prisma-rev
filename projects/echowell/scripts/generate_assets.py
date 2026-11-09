"""Generate the small original greybox atlas used by the EchoWell slice."""

from pathlib import Path
import math
import struct
import wave

from PIL import Image, ImageDraw


ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "assets" / "sprites"
AUDIO = ROOT / "assets" / "audio"


def tile_atlas() -> Image.Image:
    image = Image.new("RGBA", (64, 64), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    colors = [(24, 32, 64), (39, 58, 91), (57, 93, 112), (104, 60, 93)]
    for index, color in enumerate(colors):
        x = (index % 4) * 16
        y = (index // 4) * 16
        draw.rectangle((x, y, x + 15, y + 15), fill=color + (255,))
        draw.line((x, y, x + 15, y), fill=(116, 190, 190, 255), width=1)
        draw.line((x + 2, y + 13, x + 13, y + 3), fill=(72, 121, 132, 255), width=1)
    return image


def hero_sheet() -> Image.Image:
    image = Image.new("RGBA", (64, 24), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    for frame in range(4):
        x = frame * 16
        bob = frame % 2
        draw.rectangle((x + 4, 4 + bob, x + 11, 19 + bob), fill=(60, 220, 192, 255))
        draw.rectangle((x + 5, 2 + bob, x + 10, 7 + bob), fill=(230, 246, 226, 255))
        draw.rectangle((x + 3, 9 + bob, x + 12, 12 + bob), fill=(40, 88, 118, 255))
        draw.point((x + (7 if frame % 2 == 0 else 8), 5 + bob), fill=(20, 31, 48, 255))
    return image


def crystal() -> Image.Image:
    image = Image.new("RGBA", (16, 20), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.polygon([(8, 0), (15, 8), (11, 19), (5, 19), (1, 8)], fill=(255, 208, 92, 255))
    draw.line((8, 2, 8, 17), fill=(255, 249, 187, 255), width=2)
    draw.line((4, 9, 7, 5), fill=(255, 236, 138, 255), width=1)
    return image


def door() -> Image.Image:
    image = Image.new("RGBA", (32, 48), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.rectangle((3, 2, 28, 47), fill=(64, 41, 90, 255), outline=(167, 92, 192, 255), width=2)
    draw.rectangle((8, 9, 23, 47), fill=(18, 28, 58, 255))
    draw.rectangle((13, 24, 18, 28), fill=(255, 208, 92, 255))
    return image


def spikes() -> Image.Image:
    image = Image.new("RGBA", (16, 16), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.polygon([(0, 15), (4, 2), (8, 15)], fill=(230, 96, 112, 255))
    draw.polygon([(8, 15), (12, 2), (16, 15)], fill=(230, 96, 112, 255))
    return image


def lamp() -> Image.Image:
    image = Image.new("RGBA", (12, 16), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    draw.rectangle((4, 7, 7, 15), fill=(82, 54, 76, 255))
    draw.ellipse((1, 1, 10, 10), fill=(255, 205, 93, 220), outline=(255, 244, 165, 255))
    return image


def sound(frequency: float, duration: float, sweep: float) -> bytes:
    sample_rate = 22050
    count = int(sample_rate * duration)
    samples = bytearray()
    for index in range(count):
        time = index / sample_rate
        envelope = (1.0 - index / count) ** 2
        phase = 2.0 * math.pi * (frequency * time + sweep * time * time * 0.5)
        value = int(math.sin(phase) * envelope * 0.24 * 32767)
        samples.extend(struct.pack("<h", value))
    return bytes(samples)


def write_sound(path: Path, samples: bytes) -> None:
    with wave.open(str(path), "wb") as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(22050)
        output.writeframes(samples)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for name, image in {
        "tileset.png": tile_atlas(),
        "hero.png": hero_sheet(),
        "crystal.png": crystal(),
        "door.png": door(),
        "spikes.png": spikes(),
        "lamp.png": lamp(),
    }.items():
        image.save(OUT / name)
    AUDIO.mkdir(parents=True, exist_ok=True)
    for name, frequency, duration, sweep in [
        ("death.wav", 230.0, 0.18, -900.0),
        ("respawn.wav", 420.0, 0.20, 900.0),
        ("crystal.wav", 660.0, 0.28, 720.0),
    ]:
        write_sound(AUDIO / name, sound(frequency, duration, sweep))
    print(f"generated {len(list(OUT.glob('*.png')))} EchoWell sprites in {OUT}")


if __name__ == "__main__":
    main()
