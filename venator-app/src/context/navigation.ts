import { createContext } from "solid-js";

import { ScreenData } from "../App";

export const NavigationContext = createContext<Navigation>();

export type Navigation = {
    createTab: (data: ScreenData, navigate: boolean) => void,
    removeTab: (idx: number) => void,
    removeAllOtherTabs: (idx: number) => void,
    moveTab: (fromIdx: number, toIdx: number) => void,
    activateTab: (idx: number) => void,
}