#!/usr/bin/env python3
"""Write the SYNTHETIC Roon export fixtures (crates/pm-api/tests/fixtures/roon/).

They follow the shape of Roon's Export (select all in a browser, Export, Excel or CSV): a title row,
then a header row naming the columns the browser shows. These are not real exports: the songs are
the ones the API tests create (artist "Tester", album "Lists") plus rows that must not match.
Re-check the column names against a real export when one is in ~/roon-export/.
"""
import csv
import sys
import zipfile
from pathlib import Path
from xml.sax.saxutils import escape

OUT = Path(sys.argv[1] if len(sys.argv) > 1 else "crates/pm-api/tests/fixtures/roon")


def xlsx(path, sheet, rows):
    def col(i):
        return chr(ord("A") + i)
    cells = "".join(
        f'<row r="{r + 1}">'
        + "".join(f'<c r="{col(c)}{r + 1}" t="inlineStr"><is><t>{escape(v)}</t></is></c>' for c, v in enumerate(row))
        + "</row>"
        for r, row in enumerate(rows)
    )
    files = {
        "[Content_Types].xml": '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>',
        "_rels/.rels": '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
        "xl/workbook.xml": f'<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{escape(sheet)}" sheetId="1" r:id="rId1"/></sheets></workbook>',
        "xl/_rels/workbook.xml.rels": '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>',
        "xl/worksheets/sheet1.xml": f'<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{cells}</sheetData></worksheet>',
    }
    with zipfile.ZipFile(path, "w", zipfile.ZIP_STORED) as z:
        for name, body in files.items():
            z.writestr(zipfile.ZipInfo(name, (2020, 1, 1, 0, 0, 0)), body)


OUT.mkdir(parents=True, exist_ok=True)
xlsx(OUT / "SYNTHETIC-roon-tracks.xlsx", "Tracks", [
    ["SYNTHETIC test fixture in the shape of a Roon tracks export"],
    ["Title", "Track Artist", "Album", "Album Artist", "Track Number", "Disc Number", "Length", "Year", "Genres", "Tags", "Favorite", "Date Added", "File Path"],
    ["Tagged", "Tester", "Lists", "Tester", "1", "1", "0:01", "2020", "Disco; Funk", "late night; classics", "Yes", "2021-03-01", ""],
    ["Untagged", "Tester", "Lists", "Tester", "2", "1", "0:01", "2020", "Disco", "late night", "Yes", "2021-03-01", ""],
    ["Not In This Library", "Nobody", "Elsewhere", "Nobody", "1", "1", "3:00", "1999", "", "", "Yes", "2021-03-01", "/nowhere/song.flac"],
])
with open(OUT / "SYNTHETIC-roon-albums.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["SYNTHETIC test fixture in the shape of a Roon albums export"])
    w.writerow(["Album", "Album Artist", "Year", "Genres", "Tags", "Favorite", "Date Added", "Tracks"])
    w.writerow(["Lists", "Tester", "2020", "Disco", "classics", "Yes", "2021-03-01", "2"])
    w.writerow(["A Missing Album", "Somebody", "1990", "", "", "Yes", "2021-03-01", "10"])
xlsx(OUT / "SYNTHETIC-roon-playlist.xlsx", "Late night", [
    ["SYNTHETIC test fixture in the shape of a Roon playlist export"],
    ["Title", "Track Artist", "Album", "Length"],
    ["Tagged", "Tester", "Lists", "0:01"],
    ["Not In This Library", "Nobody", "Elsewhere", "3:00"],
])
(OUT / "README.md").write_text(
    "# SYNTHETIC Roon exports\n\nTest fixtures made by `tools/make-roon-fixtures.py` in the shape of Roon's Export "
    "(tracks, albums, a playlist). They are **not** real Roon exports. Re-check the column names against a "
    "real export when one is available in `~/roon-export/`.\n"
)
print("wrote", sorted(p.name for p in OUT.iterdir()))
