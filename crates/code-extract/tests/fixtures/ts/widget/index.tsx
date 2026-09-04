import { helper } from "../util";

/** Props accepted by the widget. */
export interface WidgetProps {
    label: string;
}

/** Render one widget. */
export function Widget(props: WidgetProps) {
    helper(props.label);
    return <div className="widget">{props.label}</div>;
}
