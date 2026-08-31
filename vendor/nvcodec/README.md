# Заголовки NVENC

`nvEncodeAPI.h` — из проекта [nv-codec-headers][repo], тег `n13.0.19.1`,
соответствует NVIDIA Video Codec SDK **13.0.19**.

## Почему не заголовки из официального SDK

Два независимых довода:

1. **Лицензия.** Этот файл распространяется под **MIT** (шапка
   в самом заголовке: «Permission is hereby granted, free of charge…»).
   Его можно вендорить в закрытый репозиторий. Заголовки из
   дистрибутива NVIDIA Video Codec SDK требуют принятия NVIDIA SDK
   License, и вендорить их нельзя (CLAUDE.md §9.1).

2. **Совместимость с драйвером.** Совместимость NVENC односторонняя:
   приложение, собранное со старым SDK, работает на новом драйвере,
   но не наоборот. Заголовки версии 13.1 на драйвере с API 13.0 дают
   `NV_ENC_ERR_INVALID_VERSION` уже на `nvEncOpenEncodeSessionEx`.
   Сборка с 13.0 работает на любом драйвере 570+, то есть покрывает
   заметно больше пользовательских машин.

## Обновление

Версия меняется вместе с минимальной поддерживаемой версией драйвера,
поэтому обновлять стоит осознанно, а не «до последней».

```powershell
$tag = "n13.0.19.1"   # см. https://github.com/FFmpeg/nv-codec-headers/tags
Invoke-WebRequest `
  "https://raw.githubusercontent.com/FFmpeg/nv-codec-headers/$tag/include/ffnvcodec/nvEncodeAPI.h" `
  -OutFile "vendor\nvcodec\nvEncodeAPI.h"
```

После обновления проверить номера версий структур в
`crates/bd-codec/src/nvenc/versions.rs` — они выписаны из заголовка
вручную, и компилятор расхождение не поймает.

## Линковка

`nvencodeapi.lib` из официального SDK **не используется**. Функции
берутся из `nvEncodeAPI64.dll`, которую ставит драйвер NVIDIA:
библиотека грузится в рантайме, поэтому сборка не требует SDK вообще,
а отсутствие NVIDIA GPU у пользователя даёт понятную ошибку вместо
отказа запуститься.

[repo]: https://github.com/FFmpeg/nv-codec-headers
