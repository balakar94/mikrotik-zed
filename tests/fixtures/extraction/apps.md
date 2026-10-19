<!-- Synthetic extraction fixture: mirrors the upstream shape (indexed H2
     headings, `---` source separators, ArgTable blocks) with invented values.
     Not a snapshot of manual.mikrotik.com. -->

## app

**Type:** Directory
<ArgTable c1="Argument">
<ArgTableRow arg="auto-update" typ="bool">Automatic updates.</ArgTableRow>
</ArgTable>

---

## app/settings

**Type:** Settings Directory
<ArgTable c1="Argument">
<ArgTableRow arg="disk" typ="string">Storage disk.</ArgTableRow>
<ArgTableRow arg="app-store-urls" typ="string">Store URLs.</ArgTableRow>
</ArgTable>

---

## Apps

**Sub-menu:** `/app`

## Properties

| Property | Type | Description |
| :-- | :-- | :-- |
| **auto-update** | bool | Automatic application updates. |

## Read-only Properties

| Property | Type | Description |
| :-- | :-- | :-- |
| **app-size** | string | Application size. |

#### Configuration

App configuration is accessible through `/app/settings`.

## Properties

| Property | Type | Description |
| :-- | :-- | :-- |
| **disk** | string | Settings storage disk. |
| **app-store-urls** | string | Settings store URLs. |
