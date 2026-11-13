import struct
import zlib
import argparse
import pathlib
from typing import List, Tuple

# Couleurs du dégradé
TOP_COLOR: Tuple[int, int, int] = (0x25, 0x63, 0xEB)
BOT_COLOR: Tuple[int, int, int] = (0x7C, 0x3A, 0xED)


def lerp(c1: Tuple[int, int, int], c2: Tuple[int, int, int], t: float) -> Tuple[int, int, int]:
    """Interpolation linéaire entre deux couleurs RGB."""
    return tuple(int(a + (b - a) * t) for a, b in zip(c1, c2))


def in_rounded_square(x: float, y: float, r: float = 0.22) -> bool:
    """Vérifie si un point normalisé (x, y) est dans un carré aux coins arrondis."""
    dx = abs(x - 0.5) - (0.5 - r)
    dy = abs(y - 0.5) - (0.5 - r)
    dx = max(dx, 0.0)
    dy = max(dy, 0.0)
    return dx * dx + dy * dy <= r * r


def in_triangle(x: float, y: float, x_min: float, x_max: float, y_min: float, y_max: float) -> bool:
    """Vérifie si un point normalisé (x, y) est dans le triangle de lecture via demi-plans."""
    if x < x_min:
        return False
    # Bord supérieur (de (x_min, y_min) à (x_max, 0.5)) : le point doit être en dessous.
    m1 = (0.5 - y_min) / (x_max - x_min)
    if y < y_min + m1 * (x - x_min):
        return False
    # Bord inférieur (de (x_min, y_max) à (x_max, 0.5)) : le point doit être au-dessus.
    m2 = (0.5 - y_max) / (x_max - x_min)
    if y > y_max + m2 * (x - x_min):
        return False
    return True


def generate_image(size: int) -> List[List[Tuple[int, int, int, int]]]:
    """Génère une image RGBA anti-aliasée pour la taille spécifiée."""
    # Configuration du triangle : 46% du côté, décalé de 4% vers la droite
    x_min = 0.5 + 0.04 - 0.23
    x_max = 0.5 + 0.04 + 0.23
    y_min = 0.5 - 0.23
    y_max = 0.5 + 0.23

    img: List[List[Tuple[int, int, int, int]]] = []
    for py in range(size):
        row: List[Tuple[int, int, int, int]] = []
        for px in range(size):
            bg_cov = 0
            fg_cov = 0
            # Suréchantillonnage 4x4
            for i in range(4):
                for j in range(4):
                    sx = (px + (i + 0.5) / 4) / size
                    sy = (py + (j + 0.5) / 4) / size
                    if in_rounded_square(sx, sy):
                        bg_cov += 1
                    if in_triangle(sx, sy, x_min, x_max, y_min, y_max):
                        fg_cov += 1

            alpha = round(bg_cov / 16.0 * 255)
            if alpha == 0:
                row.append((0, 0, 0, 0))
            else:
                t = (py + 0.5) / size
                base = lerp(TOP_COLOR, BOT_COLOR, t)
                # Triangle blanc mélangé au fond selon sa couverture (anticrénelage).
                f = min(fg_cov, bg_cov) / max(bg_cov, 1)
                color = lerp(base, (255, 255, 255), f)
                row.append((*color, alpha))
        img.append(row)
    return img


def encode_png(width: int, height: int, pixels: List[List[Tuple[int, int, int, int]]]) -> bytes:
    """Encode une image RGBA en format PNG minimal sans dépendances externes."""
    raw = bytearray()
    for row in pixels:
        raw.append(0)  # Filtre 0 (None) par ligne
        for r, g, b, a in row:
            raw.extend(struct.pack('BBBB', r, g, b, a))

    compressed = zlib.compress(bytes(raw))

    def chunk(chunk_type: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(chunk_type + data) & 0xFFFFFFFF
        return struct.pack('>I', len(data)) + chunk_type + data + struct.pack('>I', crc)

    signature = b'\x89PNG\r\n\x1a\n'
    ihdr_data = struct.pack('>IIBBBBB', width, height, 8, 6, 0, 0, 0)
    idat = chunk(b'IDAT', compressed)
    iend = chunk(b'IEND', b'')

    return signature + chunk(b'IHDR', ihdr_data) + idat + iend


def write_ico(sizes: List[int], png_data_list: List[bytes], out_path: pathlib.Path) -> None:
    """Écrit un fichier .ICO contenant les images PNG encodées."""
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with open(out_path, 'wb') as f:
        # ICONDIR : reserved=0, type=1 (ICO), count
        f.write(struct.pack('<HHH', 0, 1, len(sizes)))

        current_offset = 6 + len(sizes) * 16
        for s, png in zip(sizes, png_data_list):
            # ICONDIRENTRY : 16 octets
            w = s if s < 256 else 0
            h = s if s < 256 else 0
            f.write(struct.pack('<BBBBHHII', w, h, 0, 0, 1, 32, len(png), current_offset))
            current_offset += len(png)

        for png in png_data_list:
            f.write(png)

    print(f'écrit {out_path} ({out_path.stat().st_size} octets)')


def main() -> None:
    """Point d'entrée principal du script."""
    parser = argparse.ArgumentParser(description='Génère l\'icône .ico de GamingRemoteExperience.')
    parser.add_argument('--out', type=pathlib.Path, required=True, help='Chemin de sortie .ico')
    parser.add_argument('--png-preview', type=pathlib.Path, default=None, help='Chemin pour le PNG 256px')
    args = parser.parse_args()

    sizes = [16, 24, 32, 48, 64, 128, 256]
    png_data_list: List[bytes] = []

    for s in sizes:
        img = generate_image(s)
        png_data = encode_png(s, s, img)
        png_data_list.append(png_data)

    write_ico(sizes, png_data_list, args.out)

    if args.png_preview:
        args.png_preview.parent.mkdir(parents=True, exist_ok=True)
        with open(args.png_preview, 'wb') as f:
            f.write(png_data_list[-1])
        print(f'écrit {args.png_preview} ({args.png_preview.stat().st_size} octets)')


if __name__ == '__main__':
    main()
