// 移动端图标：从 `design/orbdrop-mobile-prototype.html` 的 `<symbol>` 表机械搬运而来
// （原样保留每个图元的属性，包括那几处 `fill="currentColor" stroke="none"` 的实心点）。
// 没有搬原型的 `<use href="#i-x">`：Svelte 里直接渲染图元更省一层，也不再依赖 id 唯一性。

/** 一个 SVG 图元。属性原样来自原型，不做归一化。 */
export type IconPrim =
  | { readonly tag: "path"; readonly attrs: Readonly<Record<string, string>> }
  | { readonly tag: "circle"; readonly attrs: Readonly<Record<string, string>> }
  | { readonly tag: "rect"; readonly attrs: Readonly<Record<string, string>> };

/** 只读形态：让下面的 `as const satisfies` 能原样保住每个图元的字面量类型。 */
export interface IconDef {
  readonly viewBox: string;
  readonly prims: readonly IconPrim[];
}

export const ICONS = {
  "chat": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "rect",
        "attrs": {
          "x": "3.75",
          "y": "5.25",
          "width": "16.5",
          "height": "13.5",
          "rx": "4.25"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "8.6",
          "cy": "12",
          "r": "1.15",
          "fill": "currentColor",
          "stroke": "none"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "12",
          "cy": "12",
          "r": "1.15",
          "fill": "currentColor",
          "stroke": "none"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "15.4",
          "cy": "12",
          "r": "1.15",
          "fill": "currentColor",
          "stroke": "none"
        }
      }
    ]
  },
  "files": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M4 15v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M12 4v10"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "m8 10 4 4 4-4"
        }
      }
    ]
  },
  "devices": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "rect",
        "attrs": {
          "x": "3",
          "y": "4.5",
          "width": "18",
          "height": "12",
          "rx": "2.2"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M9 20h6M12 16.5V20"
        }
      }
    ]
  },
  "settings": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M4 7h9M18 7h2M4 17h3M11 17h9"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "15.5",
          "cy": "7",
          "r": "2.1"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "8.5",
          "cy": "17",
          "r": "2.1"
        }
      }
    ]
  },
  "plus": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M12 5v14M5 12h14"
        }
      }
    ]
  },
  "back": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "m15 5-7 7 7 7"
        }
      }
    ]
  },
  "chev": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "m9 5 7 7-7 7"
        }
      }
    ]
  },
  "more": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "circle",
        "attrs": {
          "cx": "5",
          "cy": "12",
          "r": "1.6"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "12",
          "cy": "12",
          "r": "1.6"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "19",
          "cy": "12",
          "r": "1.6"
        }
      }
    ]
  },
  "send": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M12 19V6"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "m6 12 6-6 6 6"
        }
      }
    ]
  },
  "check": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "m5 12.5 4.5 4.5L19 7"
        }
      }
    ]
  },
  "x": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M6 6l12 12M18 6 6 18"
        }
      }
    ]
  },
  "copy": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "rect",
        "attrs": {
          "x": "9",
          "y": "9",
          "width": "11",
          "height": "11",
          "rx": "2.2"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M5.5 15V6.5a2 2 0 0 1 2-2H15"
        }
      }
    ]
  },
  "share": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "circle",
        "attrs": {
          "cx": "6",
          "cy": "12",
          "r": "2.4"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "18",
          "cy": "6",
          "r": "2.4"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "18",
          "cy": "18",
          "r": "2.4"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M8.2 10.9 15.8 7.1M8.2 13.1l7.6 3.8"
        }
      }
    ]
  },
  "trash": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M4 7h16M9 7V5h6v2M6 7l1 13h10l1-13"
        }
      }
    ]
  },
  "folder": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M3 7.5A2 2 0 0 1 5 5.5h4l2 2.4h6a2 2 0 0 1 2 2v8.6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z"
        }
      }
    ]
  },
  "file": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M6 3h8l5 5v13H6V3Z"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M14 3v5h5"
        }
      }
    ]
  },
  "image": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "rect",
        "attrs": {
          "x": "3.5",
          "y": "5",
          "width": "17",
          "height": "14",
          "rx": "2.2"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "9",
          "cy": "10",
          "r": "1.6"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "m5 17 4.5-4.5L13 16l3-2.5L20.5 18"
        }
      }
    ]
  },
  "zip": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "rect",
        "attrs": {
          "x": "5",
          "y": "4",
          "width": "14",
          "height": "6",
          "rx": "1.6"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M6.5 10v8a2 2 0 0 0 2 2h7a2 2 0 0 0 2-2v-8"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M12 3v7"
        }
      }
    ]
  },
  "download": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M12 4v10"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "m8 10 4 4 4-4"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M5 19h14"
        }
      }
    ]
  },
  "link": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M9.5 14.5 14.5 9.5"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M8 11 5.5 13.5a3.5 3.5 0 0 0 5 5L13 16"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "m16 13 2.5-2.5a3.5 3.5 0 0 0-5-5L11 8"
        }
      }
    ]
  },
  "refresh": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M4 12a8 8 0 0 1 13.7-5.7L20 8"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M20 4v4h-4"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M20 12a8 8 0 0 1-13.7 5.7L4 16"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M4 20v-4h4"
        }
      }
    ]
  },
  "bell": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M18 15V10a6 6 0 1 0-12 0v5l-1.5 3h15z"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M10 21h4"
        }
      }
    ]
  },
  "phone": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "rect",
        "attrs": {
          "x": "7",
          "y": "2.5",
          "width": "10",
          "height": "19",
          "rx": "2.6"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M11 18.5h2"
        }
      }
    ]
  },
  "bc": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M18.5 5.5 6.5 9.5H5a1.5 1.5 0 0 0-1.5 1.5v2A1.5 1.5 0 0 0 5 14.5h1.5l12 4V5.5Z"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M8 14.8V18a1.6 1.6 0 0 0 1.6 1.6h1.2"
        }
      }
    ]
  },
  "group": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M16 19v-1a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v1"
        }
      },
      {
        "tag": "circle",
        "attrs": {
          "cx": "9",
          "cy": "7",
          "r": "3"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M22 19v-1a4 4 0 0 0-3-3.87"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M16 4.13a4 4 0 0 1 0 7.75"
        }
      }
    ]
  },
  "inbox": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "path",
        "attrs": {
          "d": "M3 13h5l1.5 3h5L16 13h5"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M5.5 5h13l2.5 8v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-5z"
        }
      }
    ]
  },
  "lock": {
    "viewBox": "0 0 24 24",
    "prims": [
      {
        "tag": "rect",
        "attrs": {
          "x": "5",
          "y": "10",
          "width": "14",
          "height": "10",
          "rx": "2.2"
        }
      },
      {
        "tag": "path",
        "attrs": {
          "d": "M8 10V7a4 4 0 0 1 8 0v3"
        }
      }
    ]
  }
} as const satisfies Record<string, IconDef>;

export type IconName = keyof typeof ICONS;
