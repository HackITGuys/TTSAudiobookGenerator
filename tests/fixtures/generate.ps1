# Regenerates the PDF/EPUB/DOCX test fixtures from sample.md.
# Requires pandoc and a LaTeX distribution with pdflatex (e.g. MiKTeX).
$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    pandoc sample.md -o sample.epub
    pandoc sample.md -o sample.docx
    # report class + part division gives real part/chapter pages, page
    # numbers in the footer, footnotes at the page bottom and PDF bookmarks.
    pandoc sample.md -o sample.pdf --pdf-engine=pdflatex --top-level-division=part `
        -V documentclass=report -V papersize=a5 -V geometry:margin=2cm
    Get-Item sample.epub, sample.docx, sample.pdf | Format-Table Name, Length
}
finally {
    Pop-Location
}
