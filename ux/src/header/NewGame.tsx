import { Key, Add } from '@mui/icons-material';
import { Button, FormControl, FormControlLabel, FormGroup, FormLabel, Slider, Switch, TextField, ToggleButton, ToggleButtonGroup, Typography } from '@mui/material';
import { appWindow } from '@tauri-apps/api/window';
import React from 'react';
import { tauri_host } from '../tauri_host';

export interface NewGameProps {
  join_code: string | null,
  join_password: string | null
};
export function NewGame({ join_code, join_password }: NewGameProps) {
  const [nick, set_nick] = React.useState<null | string>(null);
  const [join_code_form, set_join_code_form] = React.useState<null | string>(null);

  const [join_or_new, set_join_or_new] = React.useState(false);
  const [is_finalizing, set_is_finalizing] = React.useState(false);
  const [is_creating, set_is_creating] = React.useState(false);
  const action = join_or_new ? "Join" : "New";
  const handle_click = async (ev: React.MouseEvent<HTMLButtonElement, MouseEvent>): Promise<void> => {
    ev.preventDefault();
    set_is_creating(true);
    if (join_or_new) {
      nick && join_code_form && await tauri_host.join_existing_game(nick, join_code_form);
    } else {
      nick && await tauri_host.make_new_game(nick);
    }
    set_is_creating(false);
  };
  const handle_finalize_click = async (ev: React.MouseEvent<HTMLButtonElement, MouseEvent>): Promise<void> => {
    ev.preventDefault();
    if (!(join_password && join_code)) return;

    set_is_finalizing(true);

    await tauri_host.finalize_game({
      passcode: join_password,
      code: join_code,
      finish_time: 60 /*minutes */ * 60 /*seconds*/ * 1000,
      start_amount: 1_000_000
    });
    set_is_finalizing(false);
  };
  return <div>
    <FormLabel>
      <span style={{ fontWeight: join_or_new ? "bold" : "normal" }}>Join </span>
      or
      <span style={{ fontWeight: !join_or_new ? "bold" : "normal" }}> Create </span>
      New Game</FormLabel>
    <FormGroup >
      <ToggleButtonGroup value={join_or_new}
        exclusive
        onChange={(a, newValue) => {
          newValue !== null && set_join_or_new(newValue)
        }}
      >
        <ToggleButton
          value={true}>
          <Key></Key>
        </ToggleButton>

        <ToggleButton value={false}>
          <Add></Add>
        </ToggleButton>
      </ToggleButtonGroup>

      <TextField label='Chain Nickname' onChange={(ev) => set_nick(ev.target.value)}></TextField>
      {
        join_or_new && <TextField label='Join Code' onChange={(ev) => set_join_code_form(ev.target.value)}></TextField>
      }
      <Button variant="contained" type="submit" onClick={handle_click} disabled={is_creating}>
        {action} {is_creating ? "Pending..." : "Game"}
      </Button>
      {
        join_code && <Typography>Invite: {join_code}</Typography>
      }
      {
        join_password &&
        <Button variant="contained" type="submit" onClick={handle_finalize_click} disabled={is_finalizing}>
          {is_finalizing ? "Finalizing..." : "Finalize Game"}
        </Button>
      }
    </FormGroup>
  </div>;
}
