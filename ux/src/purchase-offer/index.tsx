import { Card, CardHeader, CardContent } from "@mui/material";
import Form, { FormSubmit } from "@rjsf/core";
import { invoke } from "@tauri-apps/api";
import { useEffect, useMemo, useRef, useState } from "react";

const PurchaseOfferForm = ({ subtitle }: { subtitle: string }) => {
  const [schema, setSchema] = useState<null | any>(null);

  useEffect(() => {
    (async () => {
      setSchema(await invoke("get_purchase_schema"));
    })()
  });
  console.log("purchase schema:", schema);

  const handle_submit = (data: FormSubmit) => {
    invoke("make_purchase_offer", {
      purchase_data: data.formData,
      from: "who?" // get user ID from tauri State inside command?
    });
  };

  // for creater should be extracted out into a form util
  const schema_form = useMemo<JSX.Element>(() => {
    const customFormats = { "uint128": (s: string) => { return true; } };
    if (schema)
      return <Form schema={schema} noValidate={true} liveValidate={false} onSubmit={handle_submit} customFormats={customFormats}>
        <button type="submit">Submit</button>
      </Form>;

    else
      return <div></div>
  }
    , [schema]
  )
  const uid = useRef<null | HTMLInputElement>(null);
  return schema && <Card>
    <CardHeader
      title={'Purchase?'}
      subheader={subtitle}
    >
    </CardHeader>
    <CardContent>
      <div className='MoveForm' >
        <div>
          <label>Player ID:</label>
          <input type={"number"} ref={uid}></input>
        </div>
        {schema_form}
      </div>
    </CardContent>
  </Card>;
};

export default PurchaseOfferForm;