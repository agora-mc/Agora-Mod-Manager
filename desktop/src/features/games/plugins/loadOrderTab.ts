/**
 * What the instance page's load-order tab is called, and whether it exists at all. A game with no
 * load order list has no tab; one list uses its own name; several are simply "Load order".
 */

import { useEffect, useState } from 'react';
import { listLoadOrderLists } from './api';

export interface LoadOrderTab {
  show: boolean;
  label: string;
}

/** The tab's label and whether it shows. On a failed read the tab stays, so the page can say why. */
export function useLoadOrderTab(instanceId: string): LoadOrderTab {
  const [tab, setTab] = useState<LoadOrderTab>({ show: false, label: 'Plugins' });
  useEffect(() => {
    let current = true;
    setTab({ show: false, label: 'Plugins' });
    listLoadOrderLists(instanceId)
      .then((view) => {
        if (!current) return;
        if (view.lists.length === 0) setTab({ show: false, label: 'Plugins' });
        else if (view.lists.length === 1) setTab({ show: true, label: view.lists[0].name });
        else setTab({ show: true, label: 'Load order' });
      })
      .catch(() => {
        // A failed read is not "no lists": keep the tab so the page can show the failure.
        if (current) setTab({ show: true, label: 'Plugins' });
      });
    return () => {
      current = false;
    };
  }, [instanceId]);
  return tab;
}
