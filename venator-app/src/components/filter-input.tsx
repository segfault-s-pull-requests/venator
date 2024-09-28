import { createSignal, For, Match, Switch } from "solid-js";
import { FilterPredicate } from "../invoke";

import './filter-input.css';

export type FilterInputProps = {
    predicates: FilterPredicate[],
    updatePredicates: (predicates: FilterPredicate[]) => void,
    parse: (p: string) => Promise<FilterPredicate[]>,
};

export function FilterInput(props: FilterInputProps) {
    async function onblur(this: HTMLInputElement) {
        let new_predicates = await props.parse(this.value);
        let current_predicates = props.predicates;

        this.value = "";
        props.updatePredicates([...current_predicates, ...new_predicates]);
    }

    function remove(i: number) {
        let current_predicates = props.predicates;
        let updated_predicates = [...current_predicates];
        updated_predicates.splice(i, 1);
        props.updatePredicates(updated_predicates);
    }

    function update(i: number, newPredicates: FilterPredicate[]) {
        let current_predicates = props.predicates;
        let updated_predicates = [...current_predicates];
        updated_predicates.splice(i, 1, ...newPredicates);
        props.updatePredicates(updated_predicates);
    }

    return (<div class="filter-input">
        <div class="predicate-list">
            <For each={props.predicates}>
                {(predicate, i) => <FilterInputPredicate predicate={predicate} remove={() => remove(i())} update={p => update(i(), p)} parse={props.parse} />}
            </For>
            <input onchange={onblur} placeholder="filter..." />
        </div>
    </div>);
}

export function FilterInputPredicate(props: { predicate: FilterPredicate, remove: () => void, update: (p: FilterPredicate[]) => void, parse: (p: string) => Promise<FilterPredicate[]> }) {
    return (<Switch>
        <Match when={props.predicate.property == "level" && props.predicate.property_kind == 'Inherent'}>
            <FilterInputLevelPredicate predicate={props.predicate as any} remove={props.remove} update={props.update} />
        </Match>
        <Match when={props.predicate.property_kind == 'Inherent'}>
            <FilterInputMetaPredicate predicate={props.predicate} remove={props.remove} update={props.update} parse={props.parse} />
        </Match>
        <Match when={props.predicate.property_kind == 'Attribute'}>
            <FilterInputAttributePredicate predicate={props.predicate} remove={props.remove} update={props.update} parse={props.parse} />
        </Match>
    </Switch>);
}

export function FilterInputLevelPredicate(props: { predicate: FilterPredicate & { value_kind: 'comparison' }, remove: () => void, update: (p: FilterPredicate[]) => void }) {
    function wheel(e: WheelEvent) {
        if (e.deltaY < 0.0) {
            if (props.predicate.value[1] == "TRACE") {
                props.update([{ ...props.predicate, value: ['Gte', "DEBUG"], text: ">=DEBUG" }])
            } else if (props.predicate.value[1] == "DEBUG") {
                props.update([{ ...props.predicate, value: ['Gte', "INFO"], text: ">=INFO" }])
            } else if (props.predicate.value[1] == "INFO") {
                props.update([{ ...props.predicate, value: ['Gte', "WARN"], text: ">=WARN" }])
            } else if (props.predicate.value[1] == "WARN") {
                props.update([{ ...props.predicate, value: ['Gte', "ERROR"], text: ">=ERROR" }])
            }
        } else if (e.deltaY > 0.0) {
            if (props.predicate.value[1] == "DEBUG") {
                props.update([{ ...props.predicate, value: ['Gte', "TRACE"], text: ">=TRACE" }])
            } else if (props.predicate.value[1] == "INFO") {
                props.update([{ ...props.predicate, value: ['Gte', "DEBUG"], text: ">=DEBUG" }])
            } else if (props.predicate.value[1] == "WARN") {
                props.update([{ ...props.predicate, value: ['Gte', "INFO"], text: ">=INFO" }])
            } else if (props.predicate.value[1] == "ERROR") {
                props.update([{ ...props.predicate, value: ['Gte', "WARN"], text: ">=WARN" }])
            }
        }
    }

    return (<Switch>
        <Match when={props.predicate.value[1] == "TRACE"}>
            <div class="predicate level-predicate-0" onwheel={wheel}>
                {props.predicate.text}
            </div>
        </Match>
        <Match when={props.predicate.value[1] == "DEBUG"}>
            <div class="predicate level-predicate-1" onwheel={wheel}>
                {props.predicate.text}
            </div>
        </Match>
        <Match when={props.predicate.value[1] == "INFO"}>
            <div class="predicate level-predicate-2" onwheel={wheel}>
                {props.predicate.text}
            </div>
        </Match>
        <Match when={props.predicate.value[1] == "WARN"}>
            <div class="predicate level-predicate-3" onwheel={wheel}>
                {props.predicate.text}
            </div>
        </Match>
        <Match when={props.predicate.value[1] == "ERROR"}>
            <div class="predicate level-predicate-4" onwheel={wheel}>
                {props.predicate.text}
            </div>
        </Match>
    </Switch>);
}

export function FilterInputMetaPredicate(props: { predicate: FilterPredicate, remove: () => void, update: (p: FilterPredicate[]) => void, parse: (p: string) => Promise<FilterPredicate[]> }) {
    let [focused, setFocused] = createSignal<boolean>(false);
    let [error, setError] = createSignal<string | null>(null);

    async function onfocus() {
        setFocused(true);
    }

    async function onblur(this: HTMLInputElement) {
        setFocused(false);
        try {
            let newPredicates = await props.parse(this.innerText);
            props.update(newPredicates);
        }
        catch (err) {
            setError(`${err}`);
        }
    }

    async function onkeydown(this: HTMLDivElement, e: KeyboardEvent) {
        if (e.key === "Enter") {
            e.preventDefault();
            this.blur();
        }
    }

    return (<div class="predicate meta-predicate" classList={{ focused: focused(), error: error() != null && !focused() }}>
        <div class="grip">⫴</div>
        <div class="text" contenteditable="plaintext-only" onfocus={onfocus} onblur={onblur} onkeydown={onkeydown}>{props.predicate.text}</div>
        <button onclick={props.remove}>x</button>
    </div>);
}

export function FilterInputAttributePredicate(props: { predicate: FilterPredicate, remove: () => void, update: (p: FilterPredicate[]) => void, parse: (p: string) => Promise<FilterPredicate[]> }) {
    let [focused, setFocused] = createSignal<boolean>(false);
    let [error, setError] = createSignal<string | null>(null);

    async function onfocus() {
        setFocused(true);
    }

    async function onblur(this: HTMLInputElement) {
        setFocused(false);
        try {
            let newPredicates = await props.parse(this.innerText);
            props.update(newPredicates);
        }
        catch (err) {
            setError(`${err}`);
        }
    }

    async function onkeydown(this: HTMLDivElement, e: KeyboardEvent) {
        if (e.key === "Enter") {
            e.preventDefault();
            this.blur();
        }
    }

    return (<div class="predicate attribute-predicate" classList={{ focused: focused(), error: error() != null && !focused() }}>
        <div class="grip">⫴</div>
        <div class="text" contenteditable="plaintext-only" onfocus={onfocus} onblur={onblur} onkeydown={onkeydown}>{props.predicate.text}</div>
        <button onclick={props.remove}>x</button>
    </div>);
}
