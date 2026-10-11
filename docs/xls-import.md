# Importing Excel 97-2003 workbooks

Open an `.xls` file with File → Open, drag it onto Celve, or pass it to the CLI or the MCP
`open_workbook` tool. Celve reads the workbook into a new document and lists anything it
skipped.

The import is implemented from Microsoft's public [MS-XLS] (BIFF8) and [MS-CFB] (compound file)
specifications. It reads:

- **Cells:** numbers, text (including strings split across records and East Asian text),
  booleans, errors and styled blank cells.
- **Formulas**, turned back into formula text from their stored tokens, with their last saved
  results: operators, cell and range references, references to other sheets and sheet ranges,
  whole rows and columns, shared formulas, legacy array formulas (`{=…}`), defined names, array
  constants, built-in functions, and add-in or newer functions Excel stores by name (`IFERROR`,
  `EOMONTH`…).
- **Formatting:** number formats (built-in and custom), fonts, colours (including a custom
  palette), fills, borders, alignment, wrap, indent and rotation.
- **Layout:** merged cells, column widths, row heights, hidden rows, columns and sheets, frozen
  panes, zoom, gridline/heading/zero display, right-to-left sheets.
- **Workbook:** sheet order, the active sheet, defined names and the 1904 date system.

Not imported, each reported once in the open warnings: charts and chart sheets, pictures and
drawings, notes, hyperlinks, conditional formats, data validation, AutoFilters, print areas and
titles, what-if data tables (their values are kept), Excel 4.0 macro sheets and VBA. A formula
that uses something Celve can't read (links to other workbooks, macro commands) keeps its
saved result as a plain value, and the warning counts them.

Password-protected `.xls` files and Excel 5.0/95 (BIFF5) workbooks are refused with a message.
Celve does not write `.xls`: Save opens Save As and suggests a new `.xlsx` file, and requests
to save to `.xls` return an error.

```sh
celve-cli info legacy.xls
celve-cli convert legacy.xls legacy.xlsx
```

CLI import warnings go to stderr, leaving structured output on stdout available for scripts.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/cd03cb5f-ca02-4934-a391-bb674cb8aa06
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/53989ce4-7b05-4f8d-829b-d08d6148375b
